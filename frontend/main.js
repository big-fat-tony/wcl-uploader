// Logs Uploader frontend. All protocol logic lives in Rust; this file wires the
// UI and relays parser messages between Rust and the hidden parser iframe.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const PERSONAL_LOGS_GUILD_ID = -1;

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------
const logEl = $("log");
function logLine(message) {
  const ts = new Date().toLocaleTimeString();
  logEl.textContent += `[${ts}] ${message}\n`;
  logEl.scrollTop = logEl.scrollHeight;
}
$("log-clear").addEventListener("click", () => (logEl.textContent = ""));
listen("app-log", ({ payload }) => logLine(payload.message));

// ---------------------------------------------------------------------------
// Parser iframe relay (docs/PROTOCOL.md §2)
// ---------------------------------------------------------------------------
const parserFrame = $("parser");
let parserLoaded = false;
const pending = []; // { requestId, completed, id }
let probe = null; // { resolve, timer } while checking a freshly loaded parser

window.addEventListener("message", (event) => {
  if (event.source !== parserFrame.contentWindow) return;
  const data = event.data;
  if (!data || typeof data !== "object") return;

  if (data.message === "set-warning-text") {
    logLine(`Parser warning: ${data.data}`);
    return;
  }
  if (data.message === "log-message") {
    logLine(`Parser: ${(Array.isArray(data.data) ? data.data : [data.data]).join(" ")}`);
    return;
  }
  if (probe && data.message === "get-parser-version-completed") {
    const p = probe;
    probe = null;
    clearTimeout(p.timer);
    p.resolve(data.data);
    return;
  }
  const idx = pending.findIndex(
    (p) => p.completed === data.message && (p.id === undefined || p.id === data.id)
  );
  if (idx < 0) return;
  const [req] = pending.splice(idx, 1);
  invoke("parser_response", { requestId: req.requestId, payload: data }).catch(console.error);
});

listen("parser-request", ({ payload: { requestId, payload, completed } }) => {
  if (!parserLoaded || !parserFrame.contentWindow) {
    invoke("parser_response", { requestId, error: "parser is not loaded" }).catch(console.error);
    return;
  }
  pending.push({ requestId, completed, id: payload.id });
  parserFrame.contentWindow.postMessage(payload, "*");
});

listen("parser-load", ({ payload: { url } }) => {
  parserLoaded = false;
  failPending("parser reloading");
  parserFrame.src = url;
});

function failPending(reason) {
  while (pending.length) {
    const req = pending.pop();
    invoke("parser_response", { requestId: req.requestId, error: reason }).catch(console.error);
  }
}

// The iframe fires `load` even for an error page (e.g. 401), so probe it.
parserFrame.addEventListener("load", () => {
  if (!parserFrame.src || parserFrame.src === "about:blank") return;
  const version = new Promise((resolve) => {
    const timer = setTimeout(() => {
      probe = null;
      resolve(null);
    }, 20000);
    probe = { resolve, timer };
    parserFrame.contentWindow.postMessage({ message: "get-parser-version" }, "*");
  });
  version.then((v) => {
    if (v === null) {
      parserLoaded = false;
      logLine("Parser did not respond. Are you logged in?");
      invoke("parser_failed", { message: "parser did not respond (not logged in?)" }).catch(console.error);
    } else {
      parserLoaded = true;
      logLine(`Parser ready (version ${v})`);
      invoke("parser_loaded").catch(console.error);
    }
  });
});

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let session = null; // { gameVersionId, baseUrl, user }
let gameVersions = [];
let running = false;

function fillSelect(select, items, { placeholder } = {}) {
  select.innerHTML = "";
  if (placeholder) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = placeholder;
    select.appendChild(opt);
  }
  for (const item of items) {
    const opt = document.createElement("option");
    opt.value = JSON.stringify(item.value);
    opt.textContent = item.label;
    select.appendChild(opt);
  }
}
const selectedValue = (select) => (select.value === "" ? null : JSON.parse(select.value));

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------
async function init() {
  $("version-label").textContent = await invoke("client_version");
  gameVersions = await invoke("list_game_versions");
  fillSelect(
    $("login-game-version"),
    gameVersions.map((g) => ({ value: g.id, label: g.label }))
  );
  const savedGv = localStorage.getItem("gameVersionId");
  if (savedGv) $("login-game-version").value = JSON.stringify(savedGv);
  const savedEmail = localStorage.getItem("email");
  if (savedEmail) {
    $("login-email").value = savedEmail;
    $("login-remember").checked = true;
  }
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const btn = $("login-btn");
  const err = $("login-error");
  err.hidden = true;
  btn.disabled = true;
  btn.textContent = "Logging in…";
  const gameVersionId = selectedValue($("login-game-version"));
  const email = $("login-email").value.trim();
  const password = $("login-password").value;
  try {
    session = await invoke("login", { email, password, gameVersionId });
    if ($("login-remember").checked) localStorage.setItem("email", email);
    else localStorage.removeItem("email");
    localStorage.setItem("gameVersionId", gameVersionId);
    $("login-password").value = "";
    showUploader();
  } catch (ex) {
    err.textContent = String(ex);
    err.hidden = false;
  } finally {
    btn.disabled = false;
    btn.textContent = "Log in";
  }
});

$("logout-btn").addEventListener("click", async () => {
  await invoke("logout");
  session = null;
  $("uploader-view").hidden = true;
  $("session-bar").hidden = true;
  $("login-view").hidden = false;
});

// ---------------------------------------------------------------------------
// Uploader view
// ---------------------------------------------------------------------------
function showUploader() {
  const gv = gameVersions.find((g) => g.id === session.gameVersionId);
  $("session-label").textContent = `${$("login-email").value} · ${gv ? gv.label : session.gameVersionId}`;
  $("session-bar").hidden = false;
  $("login-view").hidden = true;
  $("uploader-view").hidden = false;
  $("result-card").hidden = true;

  const user = session.user;
  fillSelect($("opt-guild"), user.guildSelectItems || []);
  fillSelect($("opt-region"), user.regionOrServerSelectItems || [], { placeholder: "Choose a region" });
  fillSelect($("opt-visibility"), user.reportVisibilitySelectItems || []);
  const savedGuild = localStorage.getItem("guildId");
  if (savedGuild) $("opt-guild").value = savedGuild;
  onGuildChange();

  const savedDir = localStorage.getItem("liveDir");
  if (savedDir) $("live-dir").value = savedDir;
  else invoke("detect_log_directory", { gameVersionId: session.gameVersionId }).then((d) => {
    if (d && !$("live-dir").value) $("live-dir").value = d;
  });
}

function onGuildChange() {
  const guildId = selectedValue($("opt-guild"));
  localStorage.setItem("guildId", $("opt-guild").value);
  const personal = guildId === PERSONAL_LOGS_GUILD_ID;
  $("opt-region-wrap").hidden = !personal;
  const tags = (session.user.reportTagSelectItems || {})[String(guildId)] || [];
  fillSelect($("opt-tag"), tags, { placeholder: tags.length ? "No tag" : "No tags available" });
  $("opt-tag").disabled = tags.length === 0;
}
$("opt-guild").addEventListener("change", onGuildChange);

function reportOptions() {
  const guildId = selectedValue($("opt-guild"));
  const guild = (session.user.guildSelectItems || []).find((g) => g.value === guildId);
  let regionOrServerId;
  if (guildId === PERSONAL_LOGS_GUILD_ID) {
    regionOrServerId = selectedValue($("opt-region"));
    if (regionOrServerId === null) throw new Error("Choose a region or server for personal logs.");
  } else {
    regionOrServerId = guild && guild.regionId != null ? guild.regionId : null;
  }
  return {
    guildId,
    regionOrServerId,
    visibility: selectedValue($("opt-visibility")) ?? 0,
    reportTagId: selectedValue($("opt-tag")),
    description: $("opt-description").value.trim(),
  };
}

// Tabs
document.querySelectorAll(".tab").forEach((tab) => {
  tab.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t === tab));
    document.querySelectorAll(".tab-panel").forEach((p) => (p.hidden = p.dataset.panel !== tab.dataset.tab));
  });
});

// File pickers
$("upload-browse").addEventListener("click", async () => {
  const startDir = $("live-dir").value || null;
  const path = await invoke("pick_log_file", { startDir });
  if (path) $("upload-path").value = path;
});
$("live-browse").addEventListener("click", async () => {
  const path = await invoke("pick_log_directory", { startDir: $("live-dir").value || null });
  if (path) {
    $("live-dir").value = path;
    localStorage.setItem("liveDir", path);
  }
});
$("live-detect").addEventListener("click", async () => {
  const d = await invoke("detect_log_directory", { gameVersionId: session.gameVersionId });
  if (d) {
    $("live-dir").value = d;
    localStorage.setItem("liveDir", d);
  } else {
    logLine("Could not find a World of Warcraft Logs directory automatically.");
  }
});

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------
function setRunning(on, title) {
  running = on;
  $("upload-start").disabled = on;
  $("live-start").disabled = on;
  $("logout-btn").disabled = on;
  $("progress-card").hidden = !on;
  $("cancel-btn").disabled = false;
  $("cancel-btn").textContent = "Cancel";
  if (on) {
    $("result-card").hidden = true;
    $("progress-title").textContent = title;
    $("progress-bar").style.width = "0%";
    for (const id of ["stat-phase", "stat-file", "stat-report"]) $(id).textContent = "–";
    $("stat-lines").textContent = "0";
    $("stat-segments").textContent = "0";
    $("stat-elapsed").textContent = "0s";
  }
}

listen("operation-progress", ({ payload: p }) => {
  if (!running) return;
  $("stat-phase").textContent = p.phase + (p.uploadingMasterInfo || p.uploadingFights ? " (uploading)" : "");
  $("stat-file").textContent = p.currentFile || "–";
  $("stat-lines").textContent = p.linesParsed.toLocaleString();
  $("stat-segments").textContent = String(p.segmentsUploaded);
  $("stat-elapsed").textContent = `${Math.round(p.elapsedMs / 1000)}s`;
  $("stat-report").textContent = p.reportCode || "–";
  const bar = $("progress-bar");
  if (p.kind === "live") {
    bar.classList.toggle("indeterminate", p.isCaughtUp);
    if (!p.isCaughtUp) bar.style.width = `${p.fileReadPercent.toFixed(1)}%`;
  } else {
    bar.classList.remove("indeterminate");
    bar.style.width = `${p.fileReadPercent.toFixed(1)}%`;
  }
});

function showResult(result) {
  const card = $("result-card");
  card.hidden = false;
  card.classList.toggle("ok", result.ok);
  card.classList.toggle("fail", !result.ok);
  const open = $("result-open");
  open.hidden = !result.reportUrl;
  if (result.ok) {
    $("result-title").textContent = result.cancelled ? "Stopped" : "Done";
    $("result-body").textContent = `Report ${result.reportCode}`;
    open.onclick = () => invoke("open_external", { url: result.reportUrl });
  } else if (result.cancelled) {
    $("result-title").textContent = "Cancelled";
    $("result-body").textContent = "";
  } else {
    $("result-title").textContent = "Failed";
    $("result-body").textContent = result.error || "Unknown error";
  }
}

async function runOperation(command, params, title) {
  let result;
  setRunning(true, title);
  try {
    result = await invoke(command, { params });
  } catch (ex) {
    result = { ok: false, cancelled: false, error: String(ex) };
  }
  setRunning(false);
  showResult(result);
}

$("upload-start").addEventListener("click", async () => {
  const filePath = $("upload-path").value.trim();
  if (!filePath) return logLine("Choose a combat log file first.");
  let report;
  try {
    report = reportOptions();
  } catch (ex) {
    return logLine(ex.message);
  }
  await runOperation("start_upload", { filePath, ...report }, "Uploading log");
});

$("live-start").addEventListener("click", async () => {
  const directoryPath = $("live-dir").value.trim();
  if (!directoryPath) return logLine("Choose the Logs directory first.");
  let report;
  try {
    report = reportOptions();
  } catch (ex) {
    return logLine(ex.message);
  }
  localStorage.setItem("liveDir", directoryPath);
  await runOperation(
    "start_live_log",
    {
      directoryPath,
      ...report,
      includeEntireFile: $("live-entire").checked,
      realTime: $("live-realtime").checked,
    },
    "Live logging"
  );
});

$("cancel-btn").addEventListener("click", async () => {
  $("cancel-btn").disabled = true;
  $("cancel-btn").textContent = "Stopping…";
  await invoke("cancel_operation");
});

init().catch((e) => logLine(`Init failed: ${e}`));
