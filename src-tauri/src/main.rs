#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod clipboard;
mod engine;
mod hotkey;
mod icons;
mod logger;
mod model;
mod mute;
mod settings;
mod sounds;
mod tray;

use engine::{Download, HistoryItem, Msg, Shared, Status};
use settings::Settings;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::ManagerExt;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UiState {
    settings: Settings,
    status: Status,
    model_installed: bool,
    model_loaded: bool,
    model_bytes: u64,
    download: Download,
    history: Vec<HistoryItem>,
    hotkeys: Vec<(&'static str, &'static str)>,
    version: &'static str,
}

pub fn emit_state(app: &AppHandle) {
    let _ = app.emit("state-changed", ());
}

/// The settings window is created on demand and destroyed on close, so no
/// WebView2 processes run while Sotto sits in the tray.
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Sotto")
        .inner_size(460.0, 700.0)
        .min_inner_size(400.0, 520.0)
        .center()
        .build();
    if let Err(e) = built {
        log::error!("could not open settings: {e}");
    }
}

fn start_download(app: &AppHandle, shared: &Arc<Shared>) {
    {
        let mut d = shared.download.lock().unwrap();
        if d.active {
            return;
        }
        *d = Download { active: true, done: 0, total: model::total_bytes(), error: None };
    }
    let (app, shared) = (app.clone(), shared.clone());
    std::thread::spawn(move || {
        let mut last = Instant::now();
        let result = model::download(|done, total| {
            {
                let mut d = shared.download.lock().unwrap();
                d.done = done;
                d.total = total;
            }
            if last.elapsed() > Duration::from_millis(250) {
                last = Instant::now();
                tray::show_download(&app, done, total);
                emit_state(&app);
            }
        });
        {
            let mut d = shared.download.lock().unwrap();
            d.active = false;
            d.error = result.as_ref().err().cloned();
        }
        if let Err(e) = &result {
            log::error!("model download failed: {e}");
        }
        // Replace the progress text; Wake then moves NeedsModel to Idle.
        tray::show_status(&app, *shared.status.lock().unwrap(), &shared.settings());
        let _ = shared.tx.send(Msg::Wake);
        emit_state(&app);
    });
}

#[tauri::command]
async fn get_state(shared: State<'_, Arc<Shared>>) -> Result<UiState, String> {
    Ok(UiState {
        settings: shared.settings(),
        status: *shared.status.lock().unwrap(),
        model_installed: model::is_installed(),
        model_loaded: shared.model_loaded(),
        model_bytes: model::total_bytes(),
        download: shared.download.lock().unwrap().clone(),
        history: shared.history.lock().unwrap().iter().cloned().collect(),
        hotkeys: hotkey::PRESETS.iter().map(|p| (p.0, p.1)).collect(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[tauri::command]
async fn list_mics() -> Result<Vec<String>, String> {
    // Enumerating devices initialises COM, so keep it off shared runtime threads.
    std::thread::spawn(audio::list_input_devices).join().map_err(|_| "mic enumeration failed".to_string())
}

#[tauri::command]
async fn save_settings(app: AppHandle, shared: State<'_, Arc<Shared>>, settings: Settings) -> Result<(), String> {
    let old = shared.settings();
    if old.hotkey != settings.hotkey {
        hotkey::set_preset(&settings.hotkey);
    }
    if old.start_with_windows != settings.start_with_windows {
        let autolaunch = app.autolaunch();
        let r = if settings.start_with_windows { autolaunch.enable() } else { autolaunch.disable() };
        r.map_err(|e| e.to_string())?;
    }
    settings::save(&settings).map_err(|e| e.to_string())?;
    *shared.settings.write().unwrap() = settings.clone();
    tray::show_status(&app, *shared.status.lock().unwrap(), &settings);
    let _ = shared.tx.send(Msg::Wake);
    emit_state(&app);
    Ok(())
}

#[tauri::command]
async fn download_model(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    start_download(&app, &shared);
    Ok(())
}

#[tauri::command]
async fn copy_text(text: String) -> Result<(), String> {
    if clipboard::copy(&text) {
        Ok(())
    } else {
        Err("clipboard is busy".into())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, wav] = args.as_slice() {
        if flag == "--bench" {
            return engine::bench(wav);
        }
    }
    logger::init();
    log::info!("Sotto {} starting", env!("CARGO_PKG_VERSION"));

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| open_settings(app)))
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .invoke_handler(tauri::generate_handler![get_state, list_mics, save_settings, download_model, copy_text])
        .setup(|app| {
            let handle = app.handle().clone();
            let first_run = settings::is_first_run();
            let s = settings::load();
            if first_run {
                let _ = settings::save(&s);
                if s.start_with_windows {
                    let _ = handle.autolaunch().enable();
                }
            }

            let (tx, rx) = mpsc::channel();
            let shared = Arc::new(Shared::new(s.clone(), tx.clone()));
            app.manage(shared.clone());
            tray::build(&handle, &s)?;
            hotkey::start(tx, &s.hotkey);
            {
                let (handle, shared) = (handle.clone(), shared.clone());
                std::thread::Builder::new()
                    .name("engine".into())
                    .spawn(move || engine::run(handle, shared, rx))?;
            }
            if !model::is_installed() {
                start_download(&handle, &shared);
                open_settings(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build app");

    app.run(|_app, event| {
        // Closing the settings window must not quit the tray app.
        if let RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}
