# Sotto

Push-to-talk dictation for Windows that runs entirely on your PC. Hold a
hotkey, speak, let go, and the text is pasted wherever your cursor is. No
account, no subscription, and no audio leaves the machine.

Status: early (v0.1). Works on Windows 10/11 x64. English plus 24 European
languages (whatever Parakeet v3 supports).

## Why it's light

A dictation app runs all day, so idle cost matters more than anything else.

- **Idle:** no mic open, no model in memory, no browser engine. Measured on a
  Ryzen 9 9900X: 16 MB working set, 3.6 MB private memory, 5 threads, 16 ms
  of CPU over 60 s (0.03% of one core).
- **The mic opens only while the hotkey is held.** No privacy light, no
  Bluetooth headset stuck in call mode.
- **The model loads while you speak** (about 1.8 s, hidden behind your
  speech). It unloads after 15 minutes idle, which is configurable. About
  800 MB while loaded.
- **Speed:** 12 s of speech transcribes in about 750 ms on CPU, with no GPU
  needed.
- **Settings is a window you open, not a page that's always running.** It is
  destroyed on close.

## Using it

Hold **Ctrl + Win**, talk, release. The tray icon turns red while
listening and amber while transcribing. Left-click the tray icon for settings;
right-click for "Copy last transcript".

Settings: shortcut (Ctrl+Win, Right Ctrl, Right Alt, Ctrl+Alt, Ctrl+Shift),
microphone, mute other apps while dictating (stops YouTube or music leaking
into the mic), cue sounds, trailing space, how long to keep the model loaded,
start with Windows.

Pressing any other key while the chord is held cancels, so Ctrl+Win+D and
similar shortcuts still work.

## How it works

| Piece | File | Notes |
| --- | --- | --- |
| Hotkey | `src-tauri/src/hotkey.rs` | Low-level keyboard hook, so modifier-only chords and key-up work. Suppresses the Start menu after Win chords. |
| Mic | `src-tauri/src/audio.rs` | cpal/WASAPI, opened on press, closed on release. Windowed-sinc resample to 16 kHz. Splits long audio at quiet points. |
| Speech | `src-tauri/src/engine.rs` | NVIDIA Parakeet TDT 0.6B v3 (int8 ONNX) via `transcribe-rs`, on CPU. |
| Paste | `src-tauri/src/clipboard.rs` | Snapshot every clipboard format, paste with Ctrl+V, restore after 400 ms unless you copied something new. Writes are hidden from Win+V history. |
| Mute | `src-tauri/src/mute.rs` | Per-app mute of sessions that are playing; crash-safe (restored on next start). |
| Model | `src-tauri/src/model.rs` | 670 MB, downloaded once from a pinned Hugging Face commit, SHA-256 checked, resumable. |

Data lives in `%APPDATA%\Sotto\settings.json` and `%LOCALAPPDATA%\Sotto\`
(model and `sotto.log`). The log records timings and lengths, never transcript
text.

## Known limits

- **Admin windows:** the hotkey doesn't fire while an elevated (admin) window
  has focus. Windows blocks input hooks from normal apps into elevated ones.
  This applies to some games and anti-cheat launchers too.
- **Paste:** apps that ignore Ctrl+V won't receive text. It stays in
  "Copy last transcript" and in the settings window's Recent list.
- **Language:** no Urdu yet. A Whisper option is planned.
- **Windows only:** macOS and Linux need their own hotkey, paste and mute code.

## Build

Needs Rust (MSVC toolchain), Visual Studio Build Tools with the C++ workload,
Node 20+ and pnpm.

```bash
pnpm install
pnpm tauri build --no-bundle     # exe at src-tauri/target/release/sotto.exe
pnpm tauri build                 # NSIS installer
```

`cd src-tauri && cargo test` runs the unit tests. `scripts/e2e.ps1` runs an
end-to-end paste test. It takes keyboard focus for about 10 seconds.

`sotto.exe --bench file.wav` (16 kHz mono 16-bit) prints model load and
transcription times for a machine.

## Roadmap

1. AI cleanup pass: filler words, "no wait, Tuesday" corrections, lists.
2. "Only hears you": echo cancellation against system audio plus voice
   enrollment, so other voices and speaker output are ignored even without
   muting.
3. Signed installer and auto-update.

## Credits

- Speech model: [NVIDIA Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3),
  CC BY 4.0, ONNX export by [istupakov](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx).
- [transcribe-rs](https://github.com/cjpais/transcribe-rs) (MIT) for Parakeet inference.
- Built with [Tauri](https://tauri.app).
