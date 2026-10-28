//! The dictation loop: hotkey down -> record (model loads in parallel) ->
//! hotkey up -> transcribe on a worker thread -> paste. Runs on one thread
//! that sleeps on a channel, so idle CPU is zero. The model is dropped after
//! `keep_model_mins` of no use.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::audio::{self, Recorder, TARGET_RATE};
use crate::hotkey::{self, HotkeyEvent};
use crate::settings::Settings;
use crate::{clipboard, model, mute, sounds};

/// Keep recording briefly after release; people let go on the last syllable.
const TAIL: Duration = Duration::from_millis(150);
/// Shorter presses are treated as accidental taps.
const MIN_SECS: f32 = 0.3;
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// Below this peak level the mic is effectively silent (muted mic).
const SILENCE_PEAK: f32 = 0.003;
const HISTORY_LEN: usize = 20;

pub enum Msg {
    Hotkey(HotkeyEvent),
    Transcribed { result: Result<String, String>, audio_secs: f32, took: Duration },
    /// Settings changed; recompute timers.
    Wake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Idle,
    Recording,
    Transcribing,
    NeedsModel,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub text: String,
    pub at: u64,
    pub audio_secs: f32,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub active: bool,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

pub struct Shared {
    pub settings: RwLock<Settings>,
    pub status: Mutex<Status>,
    pub history: Mutex<VecDeque<HistoryItem>>,
    pub download: Mutex<Download>,
    pub tx: Sender<Msg>,
    model: Mutex<Option<ParakeetModel>>,
    loading: AtomicBool,
}

fn load_model() -> Result<ParakeetModel, String> {
    let t = Instant::now();
    let m = ParakeetModel::load(&model::dir(), &Quantization::Int8).map_err(|e| e.to_string())?;
    log::info!("model loaded in {} ms", t.elapsed().as_millis());
    Ok(m)
}

impl Shared {
    pub fn new(settings: Settings, tx: Sender<Msg>) -> Self {
        Self {
            settings: RwLock::new(settings),
            status: Mutex::new(Status::Idle),
            history: Mutex::new(VecDeque::new()),
            download: Mutex::new(Download::default()),
            tx,
            model: Mutex::new(None),
            loading: AtomicBool::new(false),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    /// Starts loading the model in the background while the user speaks.
    fn preload(self: &Arc<Self>) {
        if self.loading.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = self.clone();
        std::thread::spawn(move || {
            {
                let mut slot = shared.model.lock().unwrap();
                if slot.is_none() {
                    match load_model() {
                        Ok(m) => *slot = Some(m),
                        Err(e) => log::error!("model load failed: {e}"),
                    }
                }
            }
            shared.loading.store(false, Ordering::SeqCst);
        });
    }

    fn transcribe(&self, samples: &[f32]) -> Result<String, String> {
        let mut slot = self.model.lock().unwrap();
        if slot.is_none() {
            *slot = Some(load_model()?);
        }
        let model = slot.as_mut().expect("model loaded above");
        let mut parts = Vec::new();
        for chunk in audio::split_for_asr(samples, 60.0) {
            let r = model.transcribe_with(chunk, &ParakeetParams::default()).map_err(|e| e.to_string())?;
            let text = r.text.trim();
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
        Ok(parts.join(" "))
    }

    fn unload(&self) {
        if let Ok(mut slot) = self.model.try_lock() {
            if slot.take().is_some() {
                log::info!("model unloaded");
            }
        }
    }

    pub fn model_loaded(&self) -> bool {
        self.model.try_lock().map(|s| s.is_some()).unwrap_or(true)
    }
}

pub fn set_status(app: &AppHandle, shared: &Shared, status: Status) {
    {
        let mut cur = shared.status.lock().unwrap();
        if *cur == status {
            return;
        }
        *cur = status;
    }
    crate::tray::show_status(app, status, &shared.settings());
    crate::emit_state(app);
}

struct Engine {
    app: AppHandle,
    shared: Arc<Shared>,
    recording: Option<(Recorder, Instant)>,
    muted: Option<mute::Muted>,
    pending: usize,
    last_used: Instant,
}

impl Engine {
    fn idle_status(&self) -> Status {
        if self.recording.is_some() {
            Status::Recording
        } else if self.pending > 0 {
            Status::Transcribing
        } else if !model::is_installed() {
            Status::NeedsModel
        } else {
            Status::Idle
        }
    }

    fn refresh_status(&self) {
        set_status(&self.app, &self.shared, self.idle_status());
    }

    fn timeout(&self) -> Duration {
        let mut wait = Duration::from_secs(3600);
        if let Some((_, started)) = &self.recording {
            wait = wait.min(MAX_RECORDING.saturating_sub(started.elapsed()));
        }
        let keep = self.shared.settings.read().unwrap().keep_model_mins;
        if keep > 0 && self.pending == 0 && self.recording.is_none() && self.shared.model_loaded() {
            let deadline = Duration::from_secs(keep as u64 * 60);
            wait = wait.min(deadline.saturating_sub(self.last_used.elapsed()));
        }
        wait.max(Duration::from_millis(50))
    }

    fn on_tick(&mut self) {
        if self.recording.as_ref().is_some_and(|(_, s)| s.elapsed() >= MAX_RECORDING) {
            log::info!("hit the {} min limit, stopping", MAX_RECORDING.as_secs() / 60);
            self.on_released();
        }
        let keep = self.shared.settings.read().unwrap().keep_model_mins;
        let expired = keep >= 0 && self.last_used.elapsed() >= Duration::from_secs(keep as u64 * 60);
        if expired && self.pending == 0 && self.recording.is_none() {
            self.shared.unload();
        }
    }

    fn on_pressed(&mut self) {
        if self.recording.is_some() {
            return;
        }
        if !model::is_installed() {
            let app = self.app.clone();
            let _ = self.app.run_on_main_thread(move || crate::open_settings(&app));
            return;
        }
        let s = self.shared.settings();
        let t = Instant::now();
        match Recorder::start(s.mic.as_deref()) {
            Ok(r) => {
                log::info!("mic open in {} ms", t.elapsed().as_millis());
                self.recording = Some((r, Instant::now()));
            }
            Err(e) => {
                log::error!("could not open mic: {e}");
                return;
            }
        }
        self.shared.preload();
        if s.sounds {
            sounds::start();
        }
        if hotkey::combo_uses_win(&s.hotkey) {
            hotkey::suppress_start_menu();
        }
        if s.mute_while_dictating {
            self.muted = Some(mute::mute_others());
        }
        self.refresh_status();
    }

    fn stop_recording(&mut self) -> Option<(Vec<f32>, u32)> {
        let (rec, _) = self.recording.take()?;
        let out = rec.stop();
        if let Some(m) = self.muted.take() {
            m.restore();
        }
        Some(out)
    }

    fn on_released(&mut self) {
        if self.recording.is_none() {
            return;
        }
        std::thread::sleep(TAIL);
        let Some((mut samples, mut rate)) = self.stop_recording() else { return };
        // End-to-end tests swap the mic audio for a known 16 kHz recording.
        if let Some(wav) = std::env::var_os("SOTTO_TEST_WAV") {
            if let Ok(s) = transcribe_rs::audio::read_wav_samples(std::path::Path::new(&wav)) {
                (samples, rate) = (s, TARGET_RATE);
            }
        }
        let s = self.shared.settings();
        if s.sounds {
            sounds::stop();
        }
        let secs = samples.len() as f32 / rate as f32;
        if secs < MIN_SECS {
            self.refresh_status();
            return;
        }
        self.pending += 1;
        self.refresh_status();
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let audio = audio::resample(&samples, rate, TARGET_RATE);
            let result = if audio::peak(&audio) < SILENCE_PEAK {
                Ok(String::new())
            } else {
                shared.transcribe(&audio)
            };
            let _ = shared.tx.send(Msg::Transcribed { result, audio_secs: secs, took: t.elapsed() });
        });
    }

    fn on_cancelled(&mut self) {
        if self.stop_recording().is_some() {
            self.refresh_status();
        }
    }

    fn on_transcribed(&mut self, result: Result<String, String>, audio_secs: f32, took: Duration) {
        self.pending = self.pending.saturating_sub(1);
        self.last_used = Instant::now();
        let s = self.shared.settings();
        match result {
            Ok(text) if !text.is_empty() => {
                log::info!("{audio_secs:.1}s of audio -> {} chars in {} ms", text.chars().count(), took.as_millis());
                let mut out = text.clone();
                if s.trailing_space {
                    out.push(' ');
                }
                clipboard::paste(&out);
                let mut history = self.shared.history.lock().unwrap();
                history.push_front(HistoryItem {
                    text,
                    at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                    audio_secs,
                    took_ms: took.as_millis() as u64,
                });
                history.truncate(HISTORY_LEN);
            }
            Ok(_) => log::info!("{audio_secs:.1}s of audio, nothing heard"),
            Err(e) => log::error!("transcription failed: {e}"),
        }
        if s.keep_model_mins == 0 && self.pending == 0 && self.recording.is_none() {
            self.shared.unload();
        }
        self.refresh_status();
        crate::emit_state(&self.app);
    }
}

/// `sotto --bench file.wav` (16 kHz mono 16-bit): times model load and three
/// transcriptions, for checking speed on a given machine.
pub fn bench(wav: &str) {
    let samples = match transcribe_rs::audio::read_wav_samples(std::path::Path::new(wav)) {
        Ok(s) => s,
        Err(e) => return println!("can't read {wav}: {e}"),
    };
    let t = Instant::now();
    let mut model = match load_model() {
        Ok(m) => m,
        Err(e) => return println!("model load failed: {e}"),
    };
    println!("model load: {} ms", t.elapsed().as_millis());
    let secs = samples.len() as f32 / TARGET_RATE as f32;
    for run in 1..=3 {
        let t = Instant::now();
        match model.transcribe_with(&samples, &ParakeetParams::default()) {
            Ok(r) => println!("run {run}: {secs:.1}s audio in {} ms: {}", t.elapsed().as_millis(), r.text.trim()),
            Err(e) => println!("run {run} failed: {e}"),
        }
    }
}

pub fn run(app: AppHandle, shared: Arc<Shared>, rx: Receiver<Msg>) {
    // cpal opens the mic on this thread and expects an STA.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    mute::recover_after_crash();
    let mut engine = Engine {
        app,
        shared,
        recording: None,
        muted: None,
        pending: 0,
        last_used: Instant::now(),
    };
    engine.refresh_status();
    loop {
        match rx.recv_timeout(engine.timeout()) {
            Ok(Msg::Hotkey(HotkeyEvent::Pressed)) => engine.on_pressed(),
            Ok(Msg::Hotkey(HotkeyEvent::Released)) => engine.on_released(),
            Ok(Msg::Hotkey(HotkeyEvent::Cancelled)) => engine.on_cancelled(),
            Ok(Msg::Transcribed { result, audio_secs, took }) => engine.on_transcribed(result, audio_secs, took),
            Ok(Msg::Wake) => engine.refresh_status(),
            Err(RecvTimeoutError::Timeout) => engine.on_tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
