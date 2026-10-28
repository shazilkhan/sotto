# Sotto

Push-to-talk dictation for Windows that runs entirely on your PC. Hold a
hotkey, speak, let go, and the text is pasted wherever your cursor is. No
account, no subscription, and no audio leaves the machine.

Status: early (v0.3). Works on Windows 10/11 x64. English plus 24 European
languages (whatever Parakeet v3 supports).

## Why it's light

A dictation app runs all day, so idle cost matters more than anything else.

- **Idle:** no mic open, no model in memory, no browser engine. Measured on a
  Ryzen 9 9900X with the floating pill on screen: about 26 MB working set,
  8 MB private memory, 15 threads, and no measurable CPU over a minute.
- **The floating pill is not a webview.** It's a layered window drawn with
  Direct2D in software, so no GPU driver is loaded into the process and the
  GPU stays free for games. It repaints only when its state changes and
  animates only while you talk.
- **The mic opens only while the hotkey is held.** No privacy light, no
  Bluetooth headset stuck in call mode.
- **The model loads while you speak** (about 1.8 s, hidden behind your
  speech). It unloads after 15 minutes idle, which is configurable. About
  800 MB while loaded.
- **Speed:** 12 s of speech transcribes in about 750 ms on CPU, with no GPU
  needed.
- **The app window is opened on demand and destroyed on close.** Nothing
  web-based runs while Sotto waits in the tray.

## Using it

Hold **Ctrl + Win** (or the shortcut you pick), talk, release.

**Hands-free:**
- Double-tap the shortcut and Sotto keeps listening without you holding
  anything; the pill shows a stop square.
- Tap the shortcut again, or click the pill, to finish and paste.
- Esc cancels.

The floating pill shows what's happening. Idle, it's a thin underline. While
you talk, a pill lifts above it and the underline turns blue. The underline
pulses while the text is written, then turns red if something's wrong. Drag
the pill anywhere and it stays there. Right-click it for quick actions;
double-click to open Sotto. It hides behind fullscreen apps unless you're
talking.

The app window has four pages:
- **Home:** weekly words, pace and time saved, plus your latest dictations.
- **History:** every dictation, grouped by day, with search. Copy one, copy
  what you said before cleanup, or delete it.
- **Dictionary:** names and terms Sotto should spell your way, with what
  speech recognition sometimes writes instead.
  - When you fix a name right after it's pasted ("Neve" to "Niamh"),
    Sotto learns it and the pill says so.
  - It only learns capitalised names and terms that sound like what was
    heard, so grammar edits and changed meanings are ignored.
- **Settings:**
  - shortcut (Ctrl+Win, Right Ctrl, Right Alt, Ctrl+Alt, Ctrl+Shift)
  - when to show the pill
  - microphone and level test
  - mute other apps while dictating (stops YouTube or music leaking into the
    mic)
  - sounds, start with Windows, trailing space
  - how long to keep the model loaded
  - text cleanup
  - learning from corrections
  - history (on this PC only; keep 7 days, 30 days, a year or forever)

Pressing any other key while the chord is held cancels, so Ctrl+Win+D and
similar shortcuts still work.

## Text cleanup

**Light** cleanup is on by default. It's deterministic rules that take
microseconds:
- drops um, uh, ah, ooh, hmm and stretched versions ("uhhh"), and fixes the
  commas around them; meaningful words like "oh", "mhm" and "ER" stay
- collapses stutters ("I I think")
- turns a spoken "new line" or "new paragraph" into a break
- applies your Dictionary
- terminals get bare commands ("git status"), and one-line chat messages
  lose the final full stop

It never rewords. On the seed cases it passes 19 of 25 with no false edits.
The rest (self-corrections like "no wait", dictated lists) are for the
planned AI tier; see `docs/AI-CLEANUP-PLAN.md`.

## How it works

| Piece | File | Notes |
| --- | --- | --- |
| Hotkey | `src-tauri/src/hotkey.rs` | Low-level keyboard hook, so modifier-only chords and key-up work. Suppresses the Start menu after Win chords. |
| Mic | `src-tauri/src/audio.rs` | cpal/WASAPI, opened on press, closed on release. Windowed-sinc resample to 16 kHz. Splits long audio at quiet points. Feeds peak levels to the pill. |
| Speech | `src-tauri/src/engine.rs` | NVIDIA Parakeet TDT 0.6B v3 (int8 ONNX) via `transcribe-rs`, on CPU. |
| Cleanup | `src-tauri/src/cleanup.rs` | The Light rules above, plus the `--bench-cleanup` harness. |
| Pill | `src-tauri/src/pill.rs` | WS_EX_NOACTIVATE layered window, Direct2D (software) + DirectWrite, its own thread. |
| Learning | `src-tauri/src/learn.rs` | After a paste, UI Automation re-reads that one field once (never password fields) and aligns it with what was pasted. The client exists only for that check. |
| History | `src-tauri/src/transcripts.rs` | One JSON line per dictation, with retention and per-item delete. |
| Paste | `src-tauri/src/clipboard.rs` | Snapshot every clipboard format, paste with Ctrl+V, restore after 400 ms unless you copied something new. Writes are hidden from Win+V history. |
| Mute | `src-tauri/src/mute.rs` | Per-app mute of sessions that are playing; crash-safe (restored on next start). |
| Model | `src-tauri/src/model.rs` | 670 MB, downloaded once from a pinned Hugging Face commit, SHA-256 checked, resumable. |
| App window | `index.html`, `src/` | Plain TypeScript and CSS. Geist is bundled locally. |

Data lives in two places:
- `%APPDATA%\Sotto\`: settings and dictionary.
- `%LOCALAPPDATA%\Sotto\`: the model, `sotto.log`, history
  (`transcripts.jsonl`), and weekly stats (counts only).

The log records timings and lengths, never transcript text.

## Known limits

- **Admin windows:** the hotkey doesn't fire while an elevated (admin) window
  has focus. Windows blocks input hooks from normal apps into elevated ones.
  This applies to some games and anti-cheat launchers too.
- **Paste:** apps that ignore Ctrl+V won't receive text. It stays in the
  pill's "Copy last transcript" and on the Home page.
- **Learning** needs the app to expose its text box to Windows UI Automation.
  Most apps do; terminals and games usually don't. In Chromium-based apps,
  reading the field switches on their accessibility support, which costs
  them a little CPU. Turn learning off in Settings if that matters.
- **Language:** no Urdu yet. A Whisper option is planned.
- **Windows only:** macOS and Linux need their own hotkey, paste, mute and
  pill code.

## Build

Needs Rust (MSVC toolchain), Visual Studio Build Tools with the C++ workload,
Node 20+ and pnpm.

```bash
pnpm install
pnpm tauri build --no-bundle     # exe at src-tauri/target/release/sotto.exe
pnpm tauri build                 # NSIS installer
```

Checks:
- `cd src-tauri && cargo test` runs the unit tests.
- `scripts/e2e.ps1` runs an end-to-end paste test. It takes keyboard focus
  for about 10 seconds.
- `sotto.exe --bench file.wav` (16 kHz mono 16-bit) prints model load and
  transcription times.
- `sotto.exe --pill-demo`, sent to a running Sotto, walks the pill through its
  states.

Icons come from `scripts/make-icons.ps1`. It needs Geist ExtraBold as a TTF
from the [Geist release](https://github.com/vercel/geist-font/releases).

## Tuning text cleanup

- **Collect:** keep "Keep history on this PC" on in Settings. Each
  dictation is appended to `%LOCALAPPDATA%\Sotto\transcripts.jsonl` with the
  raw and cleaned text, the app's exe name (never window titles), the speech
  length and the timing. It's off by default, nothing is uploaded, and
  "Delete all" removes the file.
- **Measure:** `sotto.exe --bench-cleanup eval/cases.jsonl` runs the
  hand-written cases and prints every failure. Point it at
  `transcripts.jsonl` to see what cleanup changes in real dictations.

## Roadmap

1. AI cleanup tier: "no wait, Tuesday" corrections, lists, optional tone.
2. "Only hears you": echo cancellation against system audio plus voice
   enrollment, so other voices and speaker output are ignored even without
   muting.
3. Signed installer and auto-update.

## License

MIT, see [LICENSE](LICENSE). The speech model is a separate download under
its own license (CC BY 4.0).

## Credits

- Speech model: [NVIDIA Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3),
  CC BY 4.0, ONNX export by [istupakov](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx).
- [transcribe-rs](https://github.com/cjpais/transcribe-rs) (MIT) for Parakeet inference.
- [Geist](https://github.com/vercel/geist-font) (OFL) via Fontsource.
- Built with [Tauri](https://tauri.app).
