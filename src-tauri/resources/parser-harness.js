// Node host for the Warcraft Logs combat-log parser.
//
// The parser itself is the JavaScript the site serves at
// `/desktop-client/parser` (an inline "gamedata" script plus an external
// `parser-<game>.<hash>.js` bundle). In the official client it runs inside a
// sandboxed iframe and is driven with postMessage; here it runs in a Node `vm`
// context and is driven over stdin/stdout instead — no browser required.
//
// Protocol (newline-delimited JSON):
//   1. The first line is `{ "gamedataCode": "...", "parserCode": "..." }`.
//      Both are evaluated in this context; the harness replies
//      `{ "ready": true, "parserVersion": N }` (or `{ "ready": false, "error" }`).
//   2. Each subsequent line is `{ "action": "...", ... }`; the harness replies
//      with exactly one JSON line. The command shapes and field names mirror
//      the site's own parser-page IPC glue (ipcParseLines, ipcCollectFights,
//      ipcCollectMasterInfo, ...), so the parser globals are driven identically.

"use strict";

const vm = require("vm");

// --- Browser-ish globals the parser bundle expects --------------------------
global.window = global;
global.self = global;
// Node 18+ exposes a read-only `navigator`; override it defensively.
function define(name, value) {
  try {
    Object.defineProperty(global, name, { value, writable: true, configurable: true });
  } catch {
    try {
      global[name] = value;
    } catch {
      /* ignore */
    }
  }
}
define("navigator", { userAgent: "" });
global.document = {
  createElement: () => ({ style: {}, setAttribute() {}, appendChild() {} }),
  createElementNS: () => ({ style: {}, setAttribute() {}, appendChild() {} }),
  getElementById: () => null,
  querySelector: () => null,
  addEventListener() {},
  body: null,
};
define("location", { href: "", search: "", hash: "" });
global.addEventListener = () => {};
global.removeEventListener = () => {};
global.postMessage = () => {};

// The gamedata inline script reads these off the query string.
global.gameContentDetectionEnabled = false;
global.metersEnabled = false;
global.liveFightDataEnabled = false;
global.URLSearchParams = class {
  get(key) {
    switch (key) {
      case "id":
        return "1";
      case "gameContentDetectionEnabled":
      case "metersEnabled":
      case "liveFightDataEnabled":
        return "false";
      default:
        return null;
    }
  }
};

// The parser reports diagnostics through these; forward to stderr.
global.setWarningText = (text) => process.stderr.write(`[parser][warn] ${text}\n`);
global.setErrorText = (text) => process.stderr.write(`[parser][error] ${text}\n`);
global.sendLogMessage = (...args) => process.stderr.write(`[parser] ${args.join(" ")}\n`);
global.sendEventMessage = () => {};

// Counter the parse loop maintains (the site glue keeps it as a page global).
global.parsedLineCount = 0;

function write(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function errorMessage(e) {
  if (e && e.message) return e.message;
  const s = String(e);
  return s === "[object Object]" ? "unknown parser error" : s;
}

// --- Command handling -------------------------------------------------------
// Each handler mirrors the matching ipc* function from the site's parser page.
function handle(cmd) {
  switch (cmd.action) {
    case "get-parser-version":
      return write({
        parserVersion: typeof parserVersion !== "undefined" ? parserVersion : "unknown",
      });

    case "clear-state":
      expectedReportCode = undefined;
      clearParserState();
      global.parsedLineCount = 0;
      return write({ ok: true });

    case "clear-fights":
      logFights = { fights: [] };
      if (typeof scannedRaids !== "undefined") scannedRaids = [];
      return write({ ok: true });

    case "set-start-date":
      logStartDate = logCurrDate = cmd.startDate;
      return write({ ok: true });

    case "set-live-logging-start-time":
      liveLoggingStartTime = cmd.startTime;
      return write({ ok: true });

    case "set-report-code":
      expectedReportCode = cmd.reportCode;
      return write({ ok: true });

    case "parse-lines": {
      const lines = cmd.lines || [];
      for (let i = 0; i < lines.length; i++) {
        global.parsedLineCount++;
        try {
          parseLogLine(
            lines[i],
            cmd.scanning || false,
            cmd.selectedRegion || 0,
            cmd.raidsToUpload || [],
            cmd.logFilePosition || null,
          );
        } catch (e) {
          return write({
            success: false,
            error: errorMessage(e),
            line: lines[i],
            parsedLineCount: global.parsedLineCount,
          });
        }
      }
      return write({ success: true, parsedLineCount: global.parsedLineCount });
    }

    case "collect-fights": {
      if (cmd.pushFightIfNeeded) pushLogFight(cmd.scanningOnly || false);
      logFights.logVersion = logVersion;
      logFights.gameVersion = gameVersion;
      logFights.mythic = mythic;
      logFights.startTime = startTime;
      logFights.endTime = endTime;
      const fights = (logFights.fights || []).map((f) => ({
        eventCount: f.eventCount,
        eventsString: f.eventsString,
      }));
      return write({
        fights,
        logVersion,
        gameVersion,
        mythic,
        startTime,
        endTime,
        logFileDetails: typeof logFileDetails !== "undefined" ? logFileDetails : "",
      });
    }

    case "collect-in-progress-fight": {
      const pending =
        typeof lastAssignedEventID !== "undefined" && typeof currentEventIndex !== "undefined"
          ? lastAssignedEventID - currentEventIndex
          : 0;
      const running = typeof inCombat !== "undefined" && inCombat && pending > 1000;
      const fights = running
        ? [{ eventCount: pending, eventsString: eventsString }]
        : [];
      return write({
        fights,
        logVersion,
        gameVersion,
        mythic,
        startTime,
        endTime,
        logFileDetails: typeof logFileDetails !== "undefined" ? logFileDetails : "",
      });
    }

    case "collect-master-info": {
      if (expectedReportCode !== cmd.reportCode) {
        return write({
          success: false,
          expectedReportCode: expectedReportCode ?? null,
          actualReportCode: cmd.reportCode ?? null,
        });
      }
      buildActorsString();
      if (typeof buildAbilitiesStringIfNeeded === "function") buildAbilitiesStringIfNeeded();
      buildPetsString();
      return write({
        success: true,
        lastAssignedActorID,
        actorsString,
        lastAssignedAbilityID,
        abilitiesString,
        lastAssignedTupleID,
        tuplesString,
        lastAssignedPetID,
        petsString,
        playersString: typeof buildPlayersString === "function" ? buildPlayersString() : "",
      });
    }

    default:
      return write({ ok: false, error: `unknown action: ${cmd.action}` });
  }
}

// `expectedReportCode` is a page-level global in the site glue; declare it here
// so the handlers above can read and assign it.
var expectedReportCode = undefined;

// --- Startup: read the first line (parser code), then loop ------------------
let buffer = "";
process.stdin.setEncoding("utf-8");

function pump() {
  let nl;
  while ((nl = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, nl);
    buffer = buffer.slice(nl + 1);
    if (!line.trim()) continue;
    let cmd;
    try {
      cmd = JSON.parse(line);
    } catch (e) {
      write({ ok: false, error: `bad JSON: ${errorMessage(e)}` });
      continue;
    }
    try {
      handle(cmd);
    } catch (e) {
      write({ ok: false, error: errorMessage(e), stack: e.stack });
    }
  }
}

let started = false;
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  if (!started) {
    const nl = buffer.indexOf("\n");
    if (nl === -1) return;
    const first = buffer.slice(0, nl);
    buffer = buffer.slice(nl + 1);
    started = true;
    try {
      const payload = JSON.parse(first);
      if (payload.gamedataCode) vm.runInThisContext(payload.gamedataCode);
      if (payload.parserCode) vm.runInThisContext(payload.parserCode);
      write({
        ready: true,
        parserVersion: typeof parserVersion !== "undefined" ? parserVersion : "unknown",
      });
    } catch (e) {
      write({ ready: false, error: errorMessage(e), stack: e.stack });
      process.exit(1);
      return;
    }
  }
  pump();
});

process.stdin.on("end", () => process.exit(0));
