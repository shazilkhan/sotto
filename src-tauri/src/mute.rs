//! Mutes other apps that are playing audio while the hotkey is held, so
//! speaker output (YouTube, music, calls) can't leak into the transcript.
//! Per-app mute rather than muting the device, so our own cue sounds play.
//!
//! The muted session ids are written to disk until restored, so a crash
//! can't leave the user's apps muted.

use std::path::PathBuf;
use windows::core::Interface;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::GetCurrentProcessId;

pub struct Muted(Vec<ISimpleAudioVolume>);

fn marker_path() -> PathBuf {
    crate::settings::data_dir().join("muted-sessions.txt")
}

/// Calls `f` for every audio session on every active output device.
fn for_each_session(mut f: impl FnMut(&IAudioSessionControl2)) -> windows::core::Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..devices.GetCount()? {
            let Ok(device) = devices.Item(i) else { continue };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(session) = sessions.GetSession(j) else { continue };
                if let Ok(control) = session.cast::<IAudioSessionControl2>() {
                    f(&control);
                }
            }
        }
    }
    Ok(())
}

fn instance_id(control: &IAudioSessionControl2) -> Option<String> {
    unsafe {
        let p = control.GetSessionInstanceIdentifier().ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

pub fn mute_others() -> Muted {
    let me = unsafe { GetCurrentProcessId() };
    let mut muted = Vec::new();
    let mut ids = Vec::new();
    let result = for_each_session(|control| unsafe {
        // IsSystemSoundsSession returns S_OK for system sounds, S_FALSE otherwise.
        if control.GetProcessId().unwrap_or(0) == me || control.IsSystemSoundsSession() == S_OK {
            return;
        }
        // Only sessions making sound right now; idle apps are left alone.
        if control.GetState().map(|s| s != AudioSessionStateActive).unwrap_or(true) {
            return;
        }
        let Ok(volume) = control.cast::<ISimpleAudioVolume>() else { return };
        if volume.GetMute().map(|m| m.as_bool()).unwrap_or(true) {
            return;
        }
        if volume.SetMute(true, std::ptr::null()).is_ok() {
            if let Some(id) = instance_id(control) {
                ids.push(id);
            }
            muted.push(volume);
        }
    });
    if let Err(e) = result {
        log::warn!("muting other apps failed: {e}");
    }
    if !ids.is_empty() {
        let _ = std::fs::write(marker_path(), ids.join("\n"));
    }
    Muted(muted)
}

impl Muted {
    pub fn count(&self) -> usize {
        self.0.len()
    }

    pub fn restore(self) {
        for volume in &self.0 {
            let _ = unsafe { volume.SetMute(false, std::ptr::null()) };
        }
        if !self.0.is_empty() {
            let _ = std::fs::remove_file(marker_path());
        }
    }
}

/// Unmutes sessions left muted by a previous run that crashed mid-dictation.
pub fn recover_after_crash() {
    let Ok(text) = std::fs::read_to_string(marker_path()) else { return };
    let ids: Vec<&str> = text.lines().collect();
    let _ = for_each_session(|control| {
        if instance_id(control).is_some_and(|id| ids.contains(&id.as_str())) {
            if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                let _ = unsafe { volume.SetMute(false, std::ptr::null()) };
            }
        }
    });
    let _ = std::fs::remove_file(marker_path());
    log::info!("restored {} app(s) left muted by a previous run", ids.len());
}
