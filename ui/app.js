// Okno główne — odpowiednik MainView.swift. Stan trzyma Rust (Recorder),
// tutaj tylko rysujemy to, co przychodzi zdarzeniem `state`.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const clipboard = window.__TAURI__.clipboardManager;
const dialog = window.__TAURI__.dialog;

const $ = (id) => document.getElementById(id);
let state = { segments: [], answers: [], isRunning: false, isProcessing: false, status: "Gotowy" };
let settings = {};
let mics = { devices: [], default: null };
let selection = new Set();
let anchor = null;
let attachment = null; // { base64, url, size }
let busy = false;

// --- pomocnicze ---

function offset(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  const pad = (n) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${pad(m)}:${pad(sec)}`;
}

// Kolor awatara z nazwy: ta sama osoba ma ten sam kolor w całym transkrypcie.
// Paleta marki: akcent, jego jasna wersja i odcienie papieru; „Ty" = atrament.
const palette = [
  ["var(--accent)", "var(--dark)"],
  ["var(--bg-2)", "var(--ink)"],
  ["color-mix(in oklab, var(--accent) 40%, var(--light))", "var(--dark)"],
  ["var(--tertiary-container)", "var(--on-tertiary-container)"],
  ["var(--primary-container)", "var(--ink)"],
];
function avatarStyle(name) {
  if (name === "Ty") return ["var(--primary)", "var(--on-primary)"];
  let h = 0;
  for (const ch of name) h = ((h * 31) + ch.codePointAt(0)) & 0x7fffffff;
  return palette[h % palette.length];
}
// 1 głos, 2-4 głosy (poza 12-14), reszta głosów.
function voices(n) {
  if (n === 1) return "głos";
  return [2, 3, 4].includes(n % 10) && ![12, 13, 14].includes(n % 100) ? "głosy" : "głosów";
}
function initial(name) {
  const digits = name.replace(/\D/g, "");
  return digits ? digits.slice(0, 2) : name.slice(0, 1).toUpperCase();
}

let flashTimer;
function flash(message) {
  $("flash").textContent = "✓ " + message;
  $("flash").classList.remove("hidden");
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => $("flash").classList.add("hidden"), 3000);
}

async function copy(ids, what) {
  const text = await invoke("chat_text", { ids });
  if (!text) return;
  await clipboard.writeText(text);
  const n = ids ? ids.length : state.segments.length;
  flash("Skopiowano " + (what ?? (n === 1 ? "1 wypowiedź" : `${n} wypowiedzi`)));
}

function closeMenus() {
  document.querySelectorAll(".menu").forEach((m) => m.remove());
}

function menu(anchorEl, items) {
  closeMenus();
  const el = document.createElement("div");
  el.className = "menu";
  for (const item of items) {
    if (item === "-") { el.appendChild(document.createElement("hr")); continue; }
    const b = document.createElement("button");
    b.textContent = item.label;
    if (item.checked) b.classList.add("check");
    b.disabled = !!item.disabled;
    b.onclick = (e) => { e.stopPropagation(); closeMenus(); item.action(); };
    el.appendChild(b);
  }
  document.body.appendChild(el);
  const r = anchorEl.getBoundingClientRect();
  const w = el.offsetWidth, h = el.offsetHeight;
  el.style.left = Math.min(r.left, innerWidth - w - 8) + "px";
  el.style.top = (r.bottom + h > innerHeight ? Math.max(8, r.top - h) : r.bottom) + "px";
}
document.addEventListener("click", closeMenus);

// --- rysowanie ---

function render() {
  const s = state;
  $("title").textContent = s.title || "brqs-whisper";

  // szyna
  // Przerwanie importu / rozpoznawania głosów — w pasku stanu, tylko gdy coś trwa.
  $("cancel").classList.toggle("hidden", !s.isProcessing);
  $("r-hints").classList.toggle("selected", !!settings.assistantEnabled);
  $("r-hints").title = settings.assistantEnabled ? `Podpowiedzi włączone: ${s.assistantLabel}` : "Podpowiedzi wyłączone. Kliknij, żeby włączyć.";
  $("r-overlay").classList.toggle("selected", !!s.overlayVisible);
  $("r-copy").disabled = s.segments.length === 0;
  $("r-export").disabled = s.segments.length === 0 || s.isRunning;
  const allGood = s.readinessOk !== false;
  $("r-ready").classList.toggle("selected", !allGood);
  $("r-ready").querySelector("use").setAttribute("href", allGood ? "#i-ready" : "#i-warning");

  // baner rozmowy
  const showMeeting = s.meeting && !s.isRunning && !s.isProcessing;
  $("meeting").classList.toggle("hidden", !showMeeting);
  if (showMeeting) $("meeting-name").textContent = `Trwa rozmowa w ${s.meeting}`;

  // transkrypt
  const empty = s.segments.length === 0;
  $("empty").classList.toggle("hidden", !empty);
  $("scroll").classList.toggle("hidden", empty);
  $("empty-title").textContent = s.isRunning ? "Słucham" : s.isProcessing ? "Pracuję" : "Cisza";
  $("empty-text").textContent = s.isRunning
    ? "Tekst pojawi się, gdy ktoś powie coś dłuższego."
    : s.isProcessing ? s.status : "Kliknij „Słuchaj”, żeby zacząć zapis rozmowy, albo upuść tu nagranie lub wideo.";
  $("empty-import").classList.toggle("hidden", s.isRunning || s.isProcessing);
  if (!empty) renderSegments();

  // zaznaczenie
  $("selection-bar").classList.toggle("hidden", selection.size === 0);
  $("selection-count").textContent = selection.size === 1 ? "Zaznaczono 1 wypowiedź" : `Zaznaczono ${selection.size} wypowiedzi`;

  // źródła dźwięku i tryb
  $("src-system").classList.toggle("selected", !!settings.useSystemAudio);
  $("src-mic").classList.toggle("selected", !!settings.useMicrophone);
  const chosen = mics.devices.find((d) => d.id === settings.micDeviceId) ?? mics.default;
  $("mic-name").textContent = chosen?.name ?? "Mikrofon";
  $("src-mic").title = settings.useSystemAudio
    ? "Twój głos z mikrofonu. Na głośnikach łapie echo, lepiej na słuchawkach." : "Twój głos z mikrofonu";
  for (const id of ["src-system", "src-mic", "mic-pick"]) $(id).disabled = s.isRunning;
  $("modes").classList.toggle("hidden", s.isRunning);
  $("mode-audio").classList.toggle("selected", !settings.followObs);
  $("mode-obs").classList.toggle("selected", !!settings.followObs);

  const listen = $("listen");
  listen.classList.toggle("active", s.isRunning);
  listen.querySelector("span").textContent = s.isRunning ? "Zatrzymaj" : "Słuchaj";
  listen.querySelector("use").setAttribute("href", s.isRunning ? "#i-stop" : "#i-mic");
  listen.disabled = busy || s.isProcessing;

  renderAnswers();

  // pasek stanu
  $("progress").classList.toggle("hidden", !s.isProcessing);
  $("rec-dot").classList.toggle("hidden", !s.isRunning);
  let status = s.status;
  if ((settings.identifySpeakers || settings.diarizeAfter) && s.speakerCount > 0) {
    status += ` · ${s.speakerCount} ${voices(s.speakerCount)}`;
  }
  $("status").textContent = status;
  $("error").classList.toggle("hidden", !s.lastError);
  $("error").textContent = s.lastError ? "⚠ " + s.lastError : "";
  $("error").title = s.lastError ?? "";
  $("update").classList.toggle("hidden", !s.update);
  $("update").textContent = s.update ?? "";
}

let lastSegmentCount = 0;
function renderSegments() {
  const box = $("segments");
  const scroll = $("scroll");
  const atBottom = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 80;
  box.innerHTML = "";
  for (const seg of state.segments) {
    const [bg, fg] = avatarStyle(seg.speaker);
    const row = document.createElement("div");
    row.className = "segment" + (selection.has(seg.id) ? " selected" : "");
    row.innerHTML = `
      <div class="avatar" title="Zaznacz do skopiowania (Shift+klik: zakres)" style="background:${bg};color:${fg}">
        ${selection.has(seg.id) ? "✓" : escapeHtml(initial(seg.speaker))}</div>
      <div>
        <div class="seg-head"><b>${escapeHtml(seg.speaker)}</b><time>${offset(seg.offsetMs)}</time>
          <button class="copy" title="Kopiuj tę wypowiedź"><svg class="icon" style="width:14px;height:14px"><use href="#i-copy"/></svg></button>
          ${seg.final ? "" : '<span class="chip-live">w trakcie</span>'}</div>
        <div class="seg-text ${seg.final ? "" : "partial"}">${escapeHtml(seg.text)}</div>
      </div>`;
    row.querySelector(".avatar").onclick = (e) => toggleSelection(seg.id, e.shiftKey);
    row.querySelector(".copy").onclick = () => copy([seg.id], "wypowiedź");
    box.appendChild(row);
  }
  if (state.segments.length !== lastSegmentCount && atBottom) scroll.scrollTop = scroll.scrollHeight;
  lastSegmentCount = state.segments.length;
}

function renderAnswers() {
  const box = $("answers");
  $("assistant-label").textContent = state.assistantLabel ?? "";
  box.innerHTML = "";
  if (state.answers.length === 0) {
    const p = document.createElement("p");
    p.className = "hint";
    p.textContent = settings.assistantEnabled
      ? (settings.autoAsk ? "Pytania wykryte w rozmowie trafią tutaj same. Możesz też zapytać poniżej." : "Automatyczne pytania wyłączone. Zapytaj poniżej.")
      : "Podpowiedzi wyłączone. Włącz je w pasku po lewej.";
    box.appendChild(p);
  }
  for (const item of state.answers) box.appendChild(answerCard(item));
}
// Licznik sekund przy oczekujących odpowiedziach.
setInterval(() => tickPending($("answers")), 500);

function toggleSelection(id, shift) {
  const ids = state.segments.map((s) => s.id);
  if (shift && anchor) {
    const a = ids.indexOf(anchor), b = ids.indexOf(id);
    if (a >= 0 && b >= 0) ids.slice(Math.min(a, b), Math.max(a, b) + 1).forEach((x) => selection.add(x));
  } else if (selection.has(id)) {
    selection.delete(id);
  } else {
    selection.add(id);
  }
  anchor = id;
  render();
}

function clearSelection() { selection = new Set(); anchor = null; render(); }

// --- akcje ---

async function saveSettings(patch) {
  settings = { ...settings, ...patch };
  render();
  settings = await invoke("save_settings", { settings });
  render();
}

async function importFile(path) {
  if (!path) {
    path = await dialog.open({
      multiple: false, title: "Wybierz nagranie albo wideo do transkrypcji",
      filters: [{ name: "Nagranie albo wideo", extensions: state.mediaExtensions ?? ["mp4", "mov", "m4a", "mp3", "wav"] }],
    });
  }
  if (path) await invoke("import_file", { path });
}

async function saveAs(kind) {
  const base = state.fileBase ?? "rozmowa";
  const path = await dialog.save({ defaultPath: `${base}.${kind}`, filters: [{ name: kind, extensions: [kind] }] });
  if (!path) return;
  const contents = await invoke(kind === "md" ? "markdown" : "json_export");
  await invoke("write_file", { path, contents });
  flash("Zapisano");
}

$("cancel").onclick = () => invoke("cancel_processing");
$("empty-import").onclick = () => importFile();
$("r-hints").onclick = () => saveSettings({ assistantEnabled: !settings.assistantEnabled });
$("r-overlay").onclick = () => invoke("toggle_overlay");
$("r-folder").onclick = () => invoke("open_folder", { kind: "transcripts" });
$("r-copy").onclick = () => copy(null, "cały transkrypt");
$("r-export").onclick = (e) => {
  e.stopPropagation();
  menu($("r-export"), [
    { label: "Kopiuj Markdown", action: async () => { await clipboard.writeText(await invoke("markdown")); flash("Skopiowano Markdown"); } },
    { label: "Zapisz .md…", action: () => saveAs("md") },
    { label: "Zapisz .json…", action: () => saveAs("json") },
    "-",
    { label: "Kopiuj dla Claude (z podsumowaniem)", action: async () => {
      flash("Podsumowuję…");
      await clipboard.writeText(await invoke("claude_note"));
      flash("Skopiowano dla Claude");
    } },
  ]);
};
$("r-ready").onclick = showReadiness;
$("r-settings").onclick = showSettings;
$("meeting-listen").onclick = () => invoke("start");

$("sel-all").onclick = () => { selection = new Set(state.segments.map((s) => s.id)); render(); };
$("sel-clear").onclick = clearSelection;
$("sel-copy").onclick = async () => {
  await copy(state.segments.filter((s) => selection.has(s.id)).map((s) => s.id));
  clearSelection();
};

$("src-system").onclick = () => saveSettings({ useSystemAudio: !settings.useSystemAudio });
$("src-mic").onclick = () => saveSettings({ useMicrophone: !settings.useMicrophone });
$("mic-pick").onclick = async (e) => {
  e.stopPropagation();
  mics = await invoke("list_mics");
  const chosen = mics.devices.some((d) => d.id === settings.micDeviceId) ? settings.micDeviceId : "";
  const pick = (id) => () => saveSettings({ micDeviceId: id, useMicrophone: true });
  menu($("mic-pick"), [
    { label: mics.default ? `Domyślny systemu (${mics.default.name})` : "Domyślny systemu", checked: chosen === "", action: pick("") },
    "-",
    ...mics.devices.map((d) => ({ label: d.name, checked: chosen === d.id, action: pick(d.id) })),
  ]);
};
$("mode-audio").onclick = () => saveSettings({ followObs: false });
$("mode-obs").onclick = () => saveSettings({ followObs: true });

$("listen").onclick = async () => {
  // Blokada na czas przełączania: start podnosi whisper-server, a drugie
  // kliknięcie w międzyczasie zostawiłoby stan w połowie drogi.
  if (busy) return;
  busy = true;
  render();
  try { await invoke(state.isRunning ? "stop" : "start"); } finally { busy = false; render(); }
};

// --- pytanie ---

function canSend() {
  return settings.assistantEnabled && (attachment || $("ask").value.trim().length > 0);
}
function updateSend() { $("send").disabled = !canSend(); }

async function send() {
  if (!canSend()) return;
  const question = $("ask").value.trim();
  await invoke("ask", { question, image: attachment?.base64 ?? null });
  $("ask").value = "";
  setAttachment(null);
}

function setAttachment(value) {
  attachment = value;
  $("attachment").classList.toggle("hidden", !value);
  if (value) {
    $("attachment-img").src = value.url;
    $("attachment-size").textContent = value.size;
  }
  $("ask").placeholder = value ? "O co pytasz na tym zrzucie?" : "Zapytaj o cokolwiek…";
  updateSend();
}

// Zrzut z ekranu 4K to kilka MB; modele i tak nie korzystają z rozdzielczości
// powyżej ~1500 px, a duży base64 spowalnia prompt bardziej niż odpowiedź.
const MAX_IMAGE_SIDE = 1568;
function attachImage(url) {
  const img = new Image();
  img.onload = () => {
    const scale = Math.min(1, MAX_IMAGE_SIDE / Math.max(img.width, img.height));
    const canvas = document.createElement("canvas");
    canvas.width = Math.round(img.width * scale);
    canvas.height = Math.round(img.height * scale);
    canvas.getContext("2d").drawImage(img, 0, 0, canvas.width, canvas.height);
    const png = canvas.toDataURL("image/png");
    const kb = Math.round(png.length * 3 / 4 / 1024);
    setAttachment({ url: png, base64: png.split(",")[1], size: `${canvas.width}×${canvas.height} · ${kb} kB` });
  };
  img.src = url;
}

function imageFromBlob(blob) {
  const reader = new FileReader();
  reader.onload = () => attachImage(reader.result);
  reader.readAsDataURL(blob);
}

$("ask").addEventListener("input", () => {
  updateSend();
  $("ask").style.height = "auto";
  $("ask").style.height = $("ask").scrollHeight + "px";
});
$("ask").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || !e.shiftKey)) { e.preventDefault(); send(); }
});
// Wklejony obrazek idzie do załącznika, tekst do pola.
$("ask").addEventListener("paste", (e) => {
  const item = [...(e.clipboardData?.items ?? [])].find((i) => i.type.startsWith("image/"));
  if (item) { e.preventDefault(); imageFromBlob(item.getAsFile()); }
});
$("attach").onclick = async () => {
  const image = await invoke("clipboard_image");
  if (!image) { flash("W schowku nie ma obrazka"); return; }
  attachImage("data:image/png;base64," + image.base64);
};
$("attachment-remove").onclick = () => setAttachment(null);
$("send").onclick = send;

// --- przeciągnij i upuść = import ---

listen("tauri://drag-drop", (e) => {
  const path = e.payload.paths?.[0];
  if (path && !state.isRunning && !state.isProcessing) importFile(path);
});

// --- skróty ---

document.addEventListener("keydown", (e) => {
  const key = e.key.toLowerCase();
  if (e.ctrlKey && key === "l") { e.preventDefault(); $("listen").click(); }
  else if (e.ctrlKey && e.shiftKey && key === "c") { e.preventDefault(); if (state.segments.length) copy(null, "cały transkrypt"); }
  else if (e.ctrlKey && key === "c" && selection.size > 0 && !getSelection().toString()) { e.preventDefault(); $("sel-copy").click(); }
  else if (e.ctrlKey && key === ",") { e.preventDefault(); showSettings(); }
  else if (e.key === "Escape" && selection.size > 0) clearSelection();
});

// --- gotowość ---

async function showReadiness() {
  const checks = await invoke("readiness");
  const list = $("ready-list");
  list.innerHTML = "";
  for (const c of checks) {
    const row = document.createElement("div");
    row.className = "check-row";
    row.innerHTML = `<span>${c.ok ? "✅" : "⚠️"}</span><div class="grow"><b>${escapeHtml(c.title)}</b><small>${escapeHtml(c.detail)}</small></div>`;
    if (!c.ok && c.action) {
      const b = document.createElement("button");
      b.className = "btn-text";
      b.textContent = c.actionLabel;
      b.onclick = async () => { await invoke("readiness_action", { action: c.action }); showReadiness(); };
      row.appendChild(b);
    }
    list.appendChild(row);
  }
  if (!$("ready-dialog").open) $("ready-dialog").showModal();
}
$("ready-close").onclick = () => $("ready-dialog").close();

// --- ustawienia ---

const fields = [
  { section: "Rozmowa" },
  { key: "language", label: "Język mowy", type: "select", options: [["pl-PL", "polski"], ["en-US", "angielski (US)"], ["en-GB", "angielski (UK)"], ["de-DE", "niemiecki"]] },
  { key: "whisperModel", label: "Model whisper", type: "select", options: "models" },
  { key: "livePreview", label: "Tekst na żywo w trakcie mówienia", type: "bool", hint: "Wypowiedź przepisywana co 1,2 s, zanim się skończy. Wyłączone: tekst po końcu wypowiedzi i kilka razy mniej obciążenia." },
  { key: "whisperVocabulary", label: "Słownictwo (nazwy, technologie)", type: "text", hint: "Podawane whisperowi jako podpowiedź: „React, Next.js, Kubernetes”." },
  { key: "identifySpeakers", label: "Rozpoznawaj, kto mówi", type: "bool" },
  { key: "diarizeAfter", label: "Rozpoznawaj głosy po rozmowie (sieć neuronowa)", type: "bool" },
  { key: "projectContextPath", label: "Plik z kontekstem projektu", type: "text", hint: "Opis projektu, o którym rozmawiasz. Trafia do każdego pytania jako tło." },
  { section: "Podpowiedzi" },
  { key: "assistantEnabled", label: "Podpowiadaj w rozmowie", type: "bool" },
  { key: "autoAsk", label: "Pytania z rozmowy wysyłaj same", type: "bool" },
  { key: "assistantBackend", label: "Skąd biorą się podpowiedzi", type: "select", options: [["claudeCode", "Claude Code (bez klucza, w ramach subskrypcji)"], ["codex", "Codex / ChatGPT (bez klucza, w ramach subskrypcji ChatGPT)"], ["api", "API Experiential Labs (wymaga klucza)"]] },
  { key: "claudeModel", label: "Model Claude Code", type: "select", options: [["sonnet", "sonnet"], ["haiku", "haiku"], ["opus", "opus"]] },
  { key: "codexModel", label: "Model Codex", type: "select", options: "codexModels", hint: "Lista z konfiguracji Codexa na tym komputerze plus znane modele. Do rozmowy najlepszy najszybszy (mini). Codex instalujesz raz: `npm i -g @openai/codex`, potem `codex login` (konto ChatGPT)." },
  { key: "codexPath", label: "Ścieżka do Codex (gdy nie znajduje sam)", type: "text", hint: "Puste = szukaj samemu. W PowerShell: `where.exe codex` i wklej tu pierwszą ścieżkę." },
  { key: "codexEffort", label: "Rozumowanie Codex", type: "select", options: [["low", "niskie (najszybciej, polecane w rozmowie)"], ["medium", "średnie"], ["high", "wysokie (wolno)"]] },
  { key: "modelId", label: "Model API", type: "select", options: "apiModels" },
  { key: "apiKey", label: "Klucz API", type: "password" },
  { key: "topBar", label: "Podpowiedzi w pasku u góry ekranu", type: "bool", hint: "Odpowiednik wyspy w notchu z macOS: podpowiedź czytasz, patrząc prawie w kamerę." },
  { section: "Wykrywanie rozmów" },
  { key: "detectMeetings", label: "Powiadamiaj, gdy zaczyna się rozmowa", type: "bool" },
  { key: "autoStartOnMeeting", label: "Zaczynaj i kończ nasłuch samodzielnie", type: "bool" },
  { section: "OBS" },
  { key: "followObs", label: "Nagrywaj wideo w OBS razem z nasłuchem", type: "bool" },
  { section: "Eksport" },
  { key: "transcriptsDir", label: "Folder transkryptów", type: "text", hint: "Puste = Dokumenty\\brqs-whisper. Każda rozmowa i import zapisują się tam same jako .md; przycisk „Folder” po lewej go otwiera." },
  { key: "markdownLocale", label: "Język pliku", type: "select", options: [["pl", "polski"], ["en", "angielski"]] },
  { key: "absoluteTimestamps", label: "Znaczniki czasu jako godzina zegarowa", type: "bool" },
  { section: "Aktualizacje" },
  { key: "autoUpdate", label: "Aktualizuj z GitHuba samoczynnie", type: "bool", hint: "Co 30 minut sprawdza najnowsze wydanie i instaluje je po rozmowie. Tylko pobiera — niczego nie wysyła." },
];

async function showSettings() {
  const models = await invoke("models");
  const apiModels = await invoke("api_models");
  const codexModels = await invoke("codex_models");
  const form = $("settings-form");
  form.innerHTML = "";
  for (const f of fields) {
    if (f.section) { const h = document.createElement("h4"); h.textContent = f.section; form.appendChild(h); continue; }
    const label = document.createElement("label");
    const name = document.createElement("span");
    name.textContent = f.label;
    label.appendChild(name);
    let input;
    if (f.type === "bool") {
      input = document.createElement("input");
      input.type = "checkbox";
      input.checked = !!settings[f.key];
    } else if (f.type === "select") {
      input = document.createElement("select");
      const options = [...(f.options === "models" ? models.map((m) => [m.id, `${m.id} — ${m.note}${m.installed ? "" : " (pobierze się)"}`])
        : f.options === "apiModels" ? apiModels
        : f.options === "codexModels" ? codexModels : f.options)];
      // Zapisany model spoza listy (wpisany kiedyś ręcznie) też ma być do wyboru.
      if (settings[f.key] && !options.some(([v]) => v === settings[f.key])) options.push([settings[f.key], settings[f.key]]);
      for (const [value, text] of options) {
        const o = document.createElement("option");
        o.value = value; o.textContent = text;
        input.appendChild(o);
      }
      input.value = settings[f.key] || options[0]?.[0];
    } else {
      input = document.createElement("input");
      input.type = f.type;
      input.value = settings[f.key] ?? "";
    }
    input.dataset.key = f.key;
    label.appendChild(input);
    form.appendChild(label);
    if (f.hint) { const s = document.createElement("small"); s.textContent = f.hint; form.appendChild(s); }
  }
  const check = document.createElement("button");
  check.className = "btn-text";
  check.textContent = "Sprawdź aktualizacje teraz";
  check.onclick = () => invoke("check_update", { force: true });
  form.appendChild(check);
  $("settings-dialog").showModal();
}
$("settings-cancel").onclick = () => $("settings-dialog").close();
$("settings-save").onclick = async () => {
  const patch = {};
  for (const input of $("settings-form").querySelectorAll("[data-key]")) {
    patch[input.dataset.key] = input.type === "checkbox" ? input.checked : input.value;
  }
  await saveSettings(patch);
  $("settings-dialog").close();
};

// --- dyktowanie ---

let dict = null; // widok z Rusta (`dictation::View`)
// Nie Ctrl+J — w przeglądarkach otwiera pobrane pliki.
const DEFAULT_HOTKEY = "Ctrl+Shift+J";
let capturing = false;

// Klawisz z KeyboardEvent.code w zapisie akceleratora: KeyJ -> J, Digit1 -> 1.
function keyName(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit\d$/.test(code)) return code.slice(5);
  if (/^F\d{1,2}$/.test(code)) return code;
  const named = { Space: "Space", Enter: "Enter", Tab: "Tab", Backspace: "Backspace", Insert: "Insert", Delete: "Delete",
    Home: "Home", End: "End", PageUp: "PageUp", PageDown: "PageDown", ArrowUp: "Up", ArrowDown: "Down",
    ArrowLeft: "Left", ArrowRight: "Right", Backquote: "Backquote", Minus: "Minus", Equal: "Equal",
    BracketLeft: "BracketLeft", BracketRight: "BracketRight", Semicolon: "Semicolon", Quote: "Quote",
    Comma: "Comma", Period: "Period", Slash: "Slash", Backslash: "Backslash" };
  return named[code] ?? null;
}

async function captureHotkey(on) {
  capturing = on;
  $("d-hotkey").classList.toggle("capturing", on);
  $("d-hotkey").textContent = on ? "Naciśnij skrót…" : (dict?.hotkey ?? DEFAULT_HOTKEY);
  await invoke("dictation_capture", { on });
}

document.addEventListener("keydown", async (e) => {
  if (!capturing) return;
  e.preventDefault();
  e.stopPropagation();
  if (["Control", "Shift", "Alt", "Meta"].includes(e.key)) return;
  if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey) { await captureHotkey(false); return; }
  const key = keyName(e.code);
  const mods = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super"].filter(Boolean);
  if (!key || (mods.length === 0 && !/^F\d{1,2}$/.test(key))) {
    $("d-hotkey-error").textContent = "Dodaj modyfikator (Ctrl, Alt, Shift) albo użyj klawisza F.";
    $("d-hotkey-error").classList.remove("hidden");
    return;
  }
  capturing = false;
  $("d-hotkey").classList.remove("capturing");
  try {
    await invoke("dictation_set_hotkey", { hotkey: [...mods, key].join("+") });
  } catch (err) {
    $("d-hotkey-error").textContent = String(err);
    $("d-hotkey-error").classList.remove("hidden");
    await invoke("dictation_capture", { on: false });
  }
}, true);

function renderDictation() {
  if (!dict || !$("dictation-dialog").open) return;
  $("d-enabled").checked = dict.enabled;
  $("d-mode").value = dict.mode;
  $("d-mute").checked = dict.muteSystem;
  if (!capturing) $("d-hotkey").textContent = dict.hotkey;
  $("d-hotkey-reset").classList.toggle("hidden", dict.hotkey === DEFAULT_HOTKEY || capturing);
  $("d-lamp").classList.toggle("on", dict.hotkeyHeld);
  const err = dict.hotkeyError;
  if (err) { $("d-hotkey-error").textContent = err; $("d-hotkey-error").classList.remove("hidden"); }
  $("d-test").disabled = dict.testing;
  $("d-test").textContent = dict.testing ? "Słucham 3 s… mów teraz" : "Nagraj próbę (3 s)";
  const result = $("d-test-result");
  result.classList.toggle("hidden", !dict.testResult);
  result.classList.toggle("bad", !!dict.testResult?.startsWith("✗"));
  result.textContent = dict.testResult ?? "";
  renderDictationChecks();
}

let checksPending = false;
async function renderDictationChecks() {
  if (checksPending) return;
  checksPending = true;
  try {
    const checks = await invoke("dictation_checks");
    const list = $("d-checks");
    list.innerHTML = "";
    for (const c of checks) {
      const row = document.createElement("div");
      row.className = "check-row";
      row.innerHTML = `<span>${c.ok ? "✅" : "⚠️"}</span><div class="grow"><b>${escapeHtml(c.title)}</b><small>${escapeHtml(c.detail)}</small></div>`;
      if (!c.ok && c.action) {
        const b = document.createElement("button");
        b.className = "btn-text";
        b.textContent = c.actionLabel;
        b.onclick = () => invoke("dictation_action", { action: c.action });
        row.appendChild(b);
      }
      list.appendChild(row);
    }
  } finally {
    checksPending = false;
  }
}

async function showDictation() {
  const models = await invoke("models");
  const select = $("d-model");
  select.innerHTML = "";
  for (const m of models) {
    const o = document.createElement("option");
    o.value = m.id;
    o.textContent = `${m.id} — ${m.note}${m.installed ? "" : " (do pobrania)"}`;
    select.appendChild(o);
  }
  select.value = settings.dictationModel;
  $("d-hotkey-error").classList.add("hidden");
  dict = await invoke("dictation_state");
  $("dictation-dialog").showModal();
  renderDictation();
}

$("r-dictation").onclick = showDictation;
$("dictation-close").onclick = async () => { if (capturing) await captureHotkey(false); $("dictation-dialog").close(); };
$("dictation-dialog").addEventListener("close", () => { if (capturing) captureHotkey(false); });
$("d-hotkey").onclick = () => captureHotkey(!capturing);
$("d-hotkey-reset").onclick = () => invoke("dictation_set_hotkey", { hotkey: DEFAULT_HOTKEY });
$("d-enabled").onchange = (e) => saveSettings({ dictationEnabled: e.target.checked });
$("d-mode").onchange = (e) => saveSettings({ dictationMode: e.target.value });
$("d-mute").onchange = (e) => saveSettings({ dictationMuteSystem: e.target.checked });
$("d-model").onchange = (e) => saveSettings({ dictationModel: e.target.value });
$("d-test").onclick = () => invoke("dictation_test");
$("d-log").onclick = () => invoke("open_folder", { kind: "logs" });

// --- start ---

(async () => {
  settings = await invoke("get_settings");
  mics = await invoke("list_mics");
  state = await invoke("get_state");
  render();
  await listen("state", (e) => { state = e.payload; render(); });
  await listen("settings", (e) => { settings = e.payload; render(); });
  await listen("dictation", (e) => { dict = e.payload; renderDictation(); });
  // Braki pokazujemy od razu po otwarciu, a nie dopiero błędem po „Słuchaj".
  if (state.readinessOk === false) showReadiness();
})();
