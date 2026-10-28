import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Settings = {
  hotkey: string;
  mic: string | null;
  muteWhileDictating: boolean;
  sounds: boolean;
  trailingSpace: boolean;
  keepModelMins: number;
  startWithWindows: boolean;
  keepTranscripts: boolean;
  pillMode: "always" | "talking" | "never";
  pillX: number | null;
  pillY: number | null;
  cleanup: "off" | "light";
};

type Entry = { word: string; aliases: string[] };

type HistoryItem = { text: string; raw: string; app: string; at: number; audioSecs: number; tookMs: number };

type UiState = {
  settings: Settings;
  status: "idle" | "recording" | "transcribing" | "needsModel";
  modelInstalled: boolean;
  modelLoaded: boolean;
  modelBytes: number;
  download: { active: boolean; done: number; total: number; error: string | null };
  history: HistoryItem[];
  dictionary: Entry[];
  week: { words: number; wpm: number; savedMins: number };
  transcriptCount: number;
  hotkeys: [string, string][];
  version: string;
};

type Toggle = "muteWhileDictating" | "sounds" | "startWithWindows" | "trailingSpace" | "keepTranscripts";

const VIEWS = ["home", "dictionary", "settings"] as const;
type View = (typeof VIEWS)[number];

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const all = <T extends HTMLElement = HTMLElement>(sel: string) => [...document.querySelectorAll<T>(sel)];

let state: UiState | null = null;
let view: View = "home";
let copied = "";
let micsLoaded = false;

const CLEAN_NOTES = {
  off: "Pasted exactly as heard.",
  light:
    "Removes um and uh and stuttered words, fixes names from your Dictionary, and drops the full stop on one-line chat messages. Instant, and it runs on this PC.",
};

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function hotkeyLabel(s: UiState) {
  return s.hotkeys.find(([id]) => id === s.settings.hotkey)?.[1] ?? s.settings.hotkey;
}

function mb(bytes: number) {
  return `${Math.round(bytes / 1_000_000)} MB`;
}

function setView(next: string) {
  view = (VIEWS as readonly string[]).includes(next) ? (next as View) : "home";
  for (const v of VIEWS) $(`view-${v}`).hidden = v !== view;
  for (const a of all<HTMLAnchorElement>("nav .nav")) {
    const on = a.dataset.view === view;
    a.classList.toggle("on", on);
    if (on) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  if (location.hash !== `#${view}`) history.replaceState(null, "", `#${view}`);
  $("view-home").parentElement!.scrollTop = 0;
}

async function save(patch: Partial<Settings>) {
  if (!state) return;
  try {
    await invoke("save_settings", { settings: { ...state.settings, ...patch } });
  } catch (e) {
    alert(`Couldn't save that: ${e}`);
  }
  await refresh();
}

async function saveDictionary(entries: Entry[]) {
  try {
    await invoke("save_dictionary", { entries });
  } catch (e) {
    alert(`Couldn't save the dictionary: ${e}`);
  }
  await refresh();
}

async function copy(text: string, key: string) {
  await invoke("copy_text", { text });
  copied = key;
  if (state) renderHistory(state);
}

function renderStatus(s: UiState) {
  const text = { idle: "Ready", recording: "Listening", transcribing: "Writing", needsModel: "Setting up" }[s.status];
  $("status-text").textContent = text;
  $("status-bar").className = "status-bar" + (s.status === "recording" || s.status === "transcribing" ? " live" : "");
  $("status-mic").textContent = s.settings.mic ?? "Windows default mic";
  $("status-model").textContent = !s.modelInstalled
    ? "Downloading the speech model"
    : s.modelLoaded
      ? "Model loaded"
      : "Model loads when you talk";
}

function renderHistory(s: UiState) {
  const q = $<HTMLInputElement>("search").value.trim().toLowerCase();
  const items = s.history.filter((h) => !q || h.text.toLowerCase().includes(q) || h.app.toLowerCase().includes(q));
  const list = $("history");
  list.replaceChildren(
    ...items.map((h) => {
      const li = el("li");
      li.append(el("span", "label app", h.app || "—"), el("p", undefined, h.text));
      const meta = el("span", "meta");
      const when = new Date(h.at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
      meta.append(el("span", "time", when));
      const key = `${h.at}:text`;
      const btn = el("button", "tbtn" + (copied === key ? " done" : ""), copied === key ? "Copied" : "Copy");
      btn.type = "button";
      btn.setAttribute("aria-label", `Copy: ${h.text}`);
      btn.onclick = () => copy(h.text, key);
      meta.append(btn);
      if (h.raw && h.raw !== h.text) {
        const rkey = `${h.at}:raw`;
        const orig = el("button", "tbtn" + (copied === rkey ? " done" : ""), copied === rkey ? "Copied" : "Original");
        orig.type = "button";
        orig.title = "Copy what you said before cleanup";
        orig.onclick = () => copy(h.raw, rkey);
        meta.append(orig);
      }
      li.append(meta);
      return li;
    }),
  );
  $("history-empty").hidden = s.history.length > 0;
  if (s.history.length > 0 && items.length === 0) {
    $("history-empty").hidden = false;
    $("history-empty").textContent = "Nothing matches that.";
  } else {
    $("history-empty").textContent = "Nothing yet. Hold your shortcut and talk; your last 20 dictations show up here.";
  }
}

function renderHome(s: UiState) {
  $("home-key").textContent = hotkeyLabel(s);
  const dl = s.download;
  $("setup").hidden = s.modelInstalled;
  if (!s.modelInstalled) {
    const pct = dl.total ? Math.floor((dl.done / dl.total) * 100) : 0;
    $("setup-bar").style.width = `${pct}%`;
    $("setup-text").textContent = dl.error
      ? `The download stopped: ${dl.error}`
      : dl.active
        ? `Downloading the speech model: ${mb(dl.done)} of ${mb(dl.total)} (${pct}%). This happens once.`
        : `Sotto needs a one-time ${mb(s.modelBytes)} download.`;
    $("setup-retry").hidden = dl.active;
    $("setup-retry").textContent = dl.error ? "Try again" : "Download";
  }
  $("stat-words").textContent = s.week.words.toLocaleString();
  $("stat-wpm").textContent = String(s.week.wpm);
  $("stat-saved").textContent = String(s.week.savedMins);
  renderHistory(s);
}

function renderDictionary(s: UiState) {
  const n = s.dictionary.length;
  $("words-h").textContent = n === 0 ? "No words yet" : n === 1 ? "1 word" : `${n} words`;
  $("words").replaceChildren(
    ...s.dictionary.map((e, i) => {
      const li = el("li", "word");
      li.append(el("span", "w", e.word), el("span", "heard", e.aliases.length ? e.aliases.join("  ·  ") : "spelling only"));
      const rm = el("button", "tbtn danger", "Remove");
      rm.type = "button";
      rm.setAttribute("aria-label", `Remove ${e.word}`);
      rm.onclick = () => saveDictionary(s.dictionary.filter((_, j) => j !== i));
      li.append(rm);
      return li;
    }),
  );
  $("dict-note").textContent =
    s.settings.cleanup === "light"
      ? "Light cleanup swaps the \"heard as\" spellings as you dictate, and fixes the word's capitals."
      : "Text cleanup is off, so these aren't applied. Turn on Light in Settings.";
}

function renderSettings(s: UiState) {
  const st = s.settings;
  $("key-label").textContent = hotkeyLabel(s);
  const keySeg = $("key-seg");
  if (keySeg.childElementCount === 0) {
    for (const [id, label] of s.hotkeys) {
      const b = el("button", undefined, label);
      b.type = "button";
      b.dataset.key = id;
      b.onclick = () => {
        $("key-pick").hidden = true;
        $("key-change").textContent = "Change";
        $("key-change").classList.remove("on");
        save({ hotkey: id });
      };
      keySeg.append(b);
    }
  }
  for (const b of all<HTMLButtonElement>("#key-seg button")) {
    b.classList.toggle("on", b.dataset.key === st.hotkey);
    b.setAttribute("aria-pressed", String(b.dataset.key === st.hotkey));
  }
  for (const b of all<HTMLButtonElement>("[data-pill]")) {
    b.classList.toggle("on", b.dataset.pill === st.pillMode);
    b.setAttribute("aria-pressed", String(b.dataset.pill === st.pillMode));
  }
  $("pill-pos").textContent = st.pillX === null ? "Bottom centre of your main screen" : "Where you last left it";
  $<HTMLSelectElement>("mic").value = st.mic ?? "";
  for (const t of all<HTMLInputElement>("[data-toggle]")) t.checked = st[t.dataset.toggle as Toggle];
  $<HTMLSelectElement>("keep").value = String(st.keepModelMins);
  const dl = s.download;
  $("model-state").textContent = s.modelInstalled
    ? s.modelLoaded
      ? "Loaded"
      : "Installed"
    : dl.active && dl.total
      ? `${Math.floor((dl.done / dl.total) * 100)}%`
      : "Not downloaded";
  for (const b of all<HTMLButtonElement>("[data-clean]")) {
    b.classList.toggle("on", b.dataset.clean === st.cleanup);
    b.setAttribute("aria-pressed", String(b.dataset.clean === st.cleanup));
  }
  $("clean-note").textContent = CLEAN_NOTES[st.cleanup] ?? "";
  const n = s.transcriptCount;
  $("tx-row").hidden = !st.keepTranscripts && n === 0;
  $("tx-count").textContent = n === 1 ? "1 dictation saved" : `${n} dictations saved`;
  $("tx-clear").hidden = n === 0;
  $("version").textContent = `Sotto ${s.version}`;
}

function render(s: UiState) {
  state = s;
  renderStatus(s);
  renderHome(s);
  renderDictionary(s);
  renderSettings(s);
}

async function refresh() {
  render(await invoke<UiState>("get_state"));
}

async function loadMics() {
  if (micsLoaded) return;
  micsLoaded = true;
  const mics = await invoke<string[]>("list_mics");
  const select = $<HTMLSelectElement>("mic");
  select.replaceChildren(new Option("Windows default", ""), ...mics.map((m) => new Option(m, m)));
  if (state) select.value = state.settings.mic ?? "";
}

// Mic test: open the mic for a few seconds and draw its level.
const BARS = 24;
$("meter").replaceChildren(...Array.from({ length: BARS }, () => el("i")));
let testTimer = 0;
let testEnd = 0;

function drawMeter(level: number) {
  const lit = Math.round(level * BARS);
  all("#meter i").forEach((b, i) => b.classList.toggle("lit", i < lit));
}

async function stopTest() {
  window.clearInterval(testTimer);
  testTimer = 0;
  drawMeter(0);
  $("mic-test").textContent = "Test";
  $("mic-test").classList.remove("on");
  $("level-note").textContent = "Test it to see the mic hearing you";
  await invoke("mic_test", { on: false });
}

$("mic-test").onclick = async () => {
  if (testTimer) return stopTest();
  await invoke("mic_test", { on: true });
  $("mic-test").textContent = "Stop";
  $("mic-test").classList.add("on");
  $("level-note").textContent = "Say something";
  testEnd = Date.now() + 6000;
  testTimer = window.setInterval(async () => {
    if (Date.now() > testEnd) return stopTest();
    drawMeter(await invoke<number>("mic_level"));
  }, 70);
};

// Wiring
const win = getCurrentWindow();
$("win-min").onclick = () => win.minimize();
$("win-max").onclick = () => win.toggleMaximize();
$("win-close").onclick = () => win.close();

document.addEventListener("click", (e) => {
  const link = (e.target as HTMLElement).closest<HTMLElement>("[data-view]");
  if (link) {
    e.preventDefault();
    setView(link.dataset.view!);
  }
});

$("search").addEventListener("input", () => state && renderHistory(state));
$("setup-retry").onclick = () => invoke("download_model").then(refresh);

$<HTMLFormElement>("dict-form").addEventListener("submit", (e) => {
  e.preventDefault();
  if (!state) return;
  const word = $<HTMLInputElement>("dict-word").value.trim();
  if (!word) return;
  const aliases = $<HTMLInputElement>("dict-alias")
    .value.split(",")
    .map((a) => a.trim())
    .filter(Boolean);
  $<HTMLInputElement>("dict-word").value = "";
  $<HTMLInputElement>("dict-alias").value = "";
  saveDictionary([{ word, aliases }, ...state.dictionary.filter((d) => d.word.toLowerCase() !== word.toLowerCase())]);
});

$("key-change").onclick = () => {
  const open = $("key-pick").hidden === true;
  $("key-pick").hidden = !open;
  $("key-change").textContent = open ? "Cancel" : "Change";
  $("key-change").classList.toggle("on", open);
};

for (const b of all<HTMLButtonElement>("[data-pill]")) b.onclick = () => save({ pillMode: b.dataset.pill as Settings["pillMode"] });
for (const b of all<HTMLButtonElement>("[data-clean]")) b.onclick = () => save({ cleanup: b.dataset.clean as Settings["cleanup"] });
for (const t of all<HTMLInputElement>("[data-toggle]")) t.onchange = () => save({ [t.dataset.toggle as Toggle]: t.checked });
$("mic").onchange = () => save({ mic: $<HTMLSelectElement>("mic").value || null });
$("keep").onchange = () => save({ keepModelMins: Number($<HTMLSelectElement>("keep").value) });
$("pill-reset").onclick = () => invoke("reset_pill").then(refresh);
$("tx-show").onclick = () => invoke("show_transcripts");
$("tx-clear").onclick = async () => {
  const n = state?.transcriptCount ?? 0;
  if (!confirm(`Delete ${n} saved ${n === 1 ? "dictation" : "dictations"}? This can't be undone.`)) return;
  await invoke("clear_transcripts");
  await refresh();
};

listen("state-changed", refresh);
listen<string>("navigate", (e) => setView(e.payload));

setView(location.hash.slice(1));
refresh().then(loadMics);
