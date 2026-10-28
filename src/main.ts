import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Settings = {
  hotkey: string;
  mic: string | null;
  muteWhileDictating: boolean;
  sounds: boolean;
  trailingSpace: boolean;
  keepModelMins: number;
  startWithWindows: boolean;
};

type UiState = {
  settings: Settings;
  status: "idle" | "recording" | "transcribing" | "needsModel";
  modelInstalled: boolean;
  modelLoaded: boolean;
  modelBytes: number;
  download: { active: boolean; done: number; total: number; error: string | null };
  history: { text: string; at: number; audioSecs: number; tookMs: number }[];
  hotkeys: [string, string][];
  version: string;
};

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const toggles = ["muteWhileDictating", "sounds", "trailingSpace", "startWithWindows"] as const;

let state: UiState | null = null;

const STATUS_TEXT: Record<UiState["status"], string> = {
  idle: "Ready",
  recording: "Listening",
  transcribing: "Transcribing",
  needsModel: "Setting up",
};

function mb(bytes: number) {
  return `${Math.round(bytes / 1_000_000)} MB`;
}

function render(s: UiState) {
  state = s;
  const pill = $("status");
  pill.textContent = STATUS_TEXT[s.status];
  pill.dataset.status = s.status;

  const hotkey = $<HTMLSelectElement>("hotkey");
  if (hotkey.options.length === 0) {
    for (const [id, label] of s.hotkeys) hotkey.add(new Option(label, id));
  }
  hotkey.value = s.settings.hotkey;
  $("howto-key").textContent = s.hotkeys.find(([id]) => id === s.settings.hotkey)?.[1] ?? "";

  const mic = $<HTMLSelectElement>("mic");
  const wanted = s.settings.mic ?? "";
  if (wanted && ![...mic.options].some((o) => o.value === wanted)) mic.add(new Option(wanted, wanted));
  mic.value = wanted;

  for (const key of toggles) $<HTMLInputElement>(key).checked = s.settings[key];
  $<HTMLSelectElement>("keepModelMins").value = String(s.settings.keepModelMins);
  $("model-note").textContent = s.modelLoaded
    ? "Loaded now. Reloads while you speak, so the first word isn't lost"
    : "Not loaded now. Reloads while you speak, so the first word isn't lost";

  const dl = s.download;
  const showDownload = !s.modelInstalled;
  $("download").hidden = !showDownload;
  if (showDownload) {
    const pct = dl.total ? Math.floor((dl.done / dl.total) * 100) : 0;
    $("download-bar").style.width = `${pct}%`;
    $("download-text").textContent = dl.error
      ? `Download stopped: ${dl.error}`
      : dl.active
        ? `Downloading ${mb(dl.done)} of ${mb(dl.total)} (${pct}%). This happens once.`
        : `Needs a one-time ${mb(s.modelBytes)} download.`;
    $("download-retry").hidden = dl.active;
    $("download-retry").textContent = dl.error ? "Try again" : "Download";
  }

  const list = $("history");
  list.replaceChildren(
    ...s.history.map((h) => {
      const li = document.createElement("li");
      li.textContent = h.text;
      const meta = document.createElement("span");
      meta.className = "meta";
      const when = new Date(h.at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
      meta.textContent = `${when} · ${h.audioSecs.toFixed(1)}s of speech · ${h.tookMs} ms`;
      li.append(meta);
      li.onclick = async () => {
        await invoke("copy_text", { text: h.text });
        li.classList.add("copied");
      };
      return li;
    }),
  );
  $("history-empty").hidden = s.history.length > 0;
  $("version").textContent = `v${s.version}`;
}

async function refresh() {
  render(await invoke<UiState>("get_state"));
}

async function save() {
  if (!state) return;
  const next: Settings = {
    ...state.settings,
    hotkey: $<HTMLSelectElement>("hotkey").value,
    mic: $<HTMLSelectElement>("mic").value || null,
    keepModelMins: Number($<HTMLSelectElement>("keepModelMins").value),
  };
  for (const key of toggles) next[key] = $<HTMLInputElement>(key).checked;
  try {
    await invoke("save_settings", { settings: next });
  } catch (e) {
    alert(`Couldn't save: ${e}`);
  }
  await refresh();
}

async function loadMics() {
  const mics = await invoke<string[]>("list_mics");
  const select = $<HTMLSelectElement>("mic");
  const current = select.value;
  select.replaceChildren(new Option("Windows default", ""), ...mics.map((m) => new Option(m, m)));
  select.value = current;
}

for (const id of ["hotkey", "mic", "keepModelMins", ...toggles]) $(id).addEventListener("change", save);
$("download-retry").addEventListener("click", () => invoke("download_model").then(refresh));

listen("state-changed", refresh);
refresh().then(loadMics);
