# Warcraft Logs desktop-client upload protocol

Reconstructed from the official uploader ("Archon App" 9.6.43, Electron). All
endpoints are relative to a game-version base URL such as
`https://www.warcraftlogs.com`, `https://classic.warcraftlogs.com`,
`https://sod.warcraftlogs.com`, `https://fresh.warcraftlogs.com`, ...

Every request carries the session cookie (`wcl_session`, `XSRF-TOKEN`;
`SameSite=None; Secure`, 2 h `Max-Age`, refreshed on each response). The server
reflects any `Origin` with `Access-Control-Allow-Credentials: true`, so a
non-browser origin can use `fetch(..., { credentials: "include" })`.

## 1. Authentication

```
POST /desktop-client/log-in
Content-Type: application/json
{
  "email": "...", "password": "...",
  "version": "9.6.43",                 // client version string
  "clientTime": "2026-09-15T00:00:00.000Z",
  "gameVersionId": "warcraft-live"     // see section 5
}
```

Response `200` JSON: `{ "user": {...}, ...rest }`. The client merges
`{...rest, ...user}` into one object. Fields the uploader uses:

| field | shape |
|---|---|
| `id` | user id |
| `guildSelectItems` | `[{ value: guildId, label, regionId }]` (personal logs use a sentinel guild id) |
| `reportTagSelectItems` | `{ [guildId]: [{ value: reportTagId, label }] }` |
| `reportVisibilitySelectItems` | `[{ value: visibilityId, label }]` |
| `regionOrServerSelectItems` | `[{ value: regionOrServerId, label }]` |
| `enabledFeatures` | `{ liveFightData, video, ... }` |

`400` JSON `{ "message": "..." }` on bad credentials.

`POST /desktop-client/log-out` (empty body) ends the session.

`POST /desktop-client/token/v2` returns a JWT (text) used for side features
(user event stream / broadcasts). Not needed for uploading.

## 2. Parser

The combat-log parser is **not shipped with the client**. It is a web page
loaded into a sandboxed iframe (`sandbox="allow-scripts"`) and driven with
`postMessage`:

```
GET /desktop-client/parser?id=<iframeId>&ts=<loadTimestamp>
    &gameContentDetectionEnabled=false&metersEnabled=false
    &liveFightDataEnabled=false&gameVersionId=<gameVersionId>
```

Requires the session cookie (`401` otherwise). The client reloads the parser
iframe every hour while idle.

Request/response messages (`iframe.contentWindow.postMessage(req, "*")`; the
parser answers to `window.parent`; the client filters responses by
`event.origin === baseUrl`). Every request except the first three carries
`id: <iframeId>` and the response echoes it.

| request `message` | extra fields | response `message` / payload |
|---|---|---|
| `get-parser-version` | | `get-parser-version-completed` `{ data: string }` |
| `clear-state` | | `clear-state-completed` |
| `clear-meters` | | `clear-meters-completed` |
| `set-start-date` | `startDate: string \| ""` | `set-start-date-completed` |
| `set-live-logging-start-time` | `startTime: epochMs` | `set-live-logging-start-time-completed` |
| `set-report-code` | `reportCode` | `set-report-code-completed` |
| `parse-lines` | `lines: string[]`, `selectedRegion: regionOrServerId`, `raidsToUpload: number[]`, `scanning: bool`, `logFilePosition: { filePath, currentPosition, startingPosition }` | `parse-lines-completed` `{ success, parsedLineCount, exception, line }` |
| `collect-fights` | `pushFightIfNeeded: bool`, `scanningOnly: bool` | `collect-fights-completed` `{ fights: [{ eventCount, eventsString }], logVersion, gameVersion, startTime, endTime, mythic, logFileDetails }` |
| `collect-in-progress-fight` | | same shape as `collect-fights-completed` |
| `collect-master-info` | `reportCode` | `collect-master-info-completed` `{ success, expectedReportCode, actualReportCode, lastAssignedActorID, actorsString, lastAssignedAbilityID, abilitiesString, lastAssignedTupleID, tuplesString, lastAssignedPetID, petsString, playersString }` |
| `collect-scanned-raids` | | `collect-scanned-raids-completed` |
| `clear-fights` | | `clear-fights-completed` |
| `call-wipe` | | `call-wipe-completed` |
| `check-dungeon-inactivity` | `clientSideTime` | `{ justFired, linesPerWindow }` |
| `force-end-game-content` | | `force-end-game-content-completed` |

Unsolicited messages from the parser: `set-warning-text { data }` and
`log-message { data: any[] }`.

## 3. Upload a log (logMode 2)

1. `get-parser-version` → `parserVersion`.
2. Create the report:

   ```
   POST /desktop-client/create-report
   Content-Type: application/json
   { "clientVersion", "parserVersion", "startTime": now, "endTime": now,
     "guildId", "fileName", "serverOrRegion": regionOrServerId,
     "visibility": visibilityId, "reportTagId", "description",
     "logMode": 2 }            // 1 = live log, 3 = auto log
   ```

   → `{ "code": "<reportCode>" }`. Files above 3.5 GB are rejected client-side.
3. `set-start-date` (WoW: empty), `set-report-code`.
4. Read the file in parts (≤ 5000 lines / ≤ 8 MiB, UTF-8) and for each part:
   - `parse-lines` (`selectedRegion` = regionOrServerId or 0, `raidsToUpload` =
     specific fight ids or `[]`, `scanning = false`).
   - `collect-fights(pushFightIfNeeded = endOfFile, scanningOnly = false)`.
   - If `fights.length > 0`:
     1. `collect-master-info(reportCode)`; `success = false` means the parser
        state belongs to another report → abort.
     2. Build the master-table text and upload it (3.1).
     3. Build the fights text and upload it (3.2); the response carries
        `nextSegmentId`.
     4. `clear-fights`.
5. `clear-fights` + `clear-state`, then
   `POST /desktop-client/terminate-report/<code>` (empty body).

`segmentId` starts at 0 and is replaced by the `nextSegmentId` returned from
each `add-report-segment` response.

### 3.1 Master table

Text payload (the `*String` fields already end in newlines):

```
{logVersion}|{gameVersion}|{logFileDetails}\n
{lastAssignedActorID}\n{actorsString}
{lastAssignedAbilityID}\n{abilitiesString}
{lastAssignedTupleID}\n{tuplesString}
{lastAssignedPetID}\n{petsString}
```

Zipped as a single entry `log.txt` (DEFLATE level 9) and posted:

```
POST /desktop-client/set-report-master-table/<code>
Accept: application/json
multipart/form-data:
  segmentId  = <segmentId>
  isRealTime = "true" | "false"
  logfile    = <zip bytes>, filename "blob"
  players    = <playersString>
```

### 3.2 Fights segment

```
{logVersion}|{gameVersion}\n
{sum of eventCount}\n
{concatenated eventsString of every fight}
```

Zipped the same way and posted:

```
POST /desktop-client/add-report-segment/<code>
Accept: application/json
multipart/form-data:
  logfile    = <zip bytes>, filename "blob"
  parameters = JSON {
    startTime, endTime, mythic,       // from collect-fights
    isLiveLog, isRealTime,
    inProgressEventCount,             // 0 unless real-time live logging
    segmentId }
```

→ `{ "nextSegmentId": n, ... }`. Errors → JSON `{ "message" }`.

Retry policy for both uploads: up to 120 attempts, 30 s apart; on `401`
re-login once, then give up.

## 4. Live log (logMode 1 / 3)

Same as section 3 but the newest `^WoWCombatLog.*\.txt$` file in the log
directory is tailed: parts are read from the last position, `collect-fights`
is called with `pushFightIfNeeded = false` (true only on cancel / idle > 120 s),
and when `enableRealTimeUploading` is set, `collect-in-progress-fight` uploads
partial fights with `isRealTime = true` and `inProgressEventCount` set. The
report is terminated when live logging stops. Files older than 6 h are
ignored; a shrinking file is treated as truncation and re-read from 0.

## 5. Game versions and base URLs

`gameVersionId` → subdomain of `warcraftlogs.com`:

| id | base URL | WoW install dir |
|---|---|---|
| `warcraft-live` | `www` | `_retail_` |
| `warcraft-live-ptr` / `-xptr` / `-beta` | `www` (ptr sites) | `_ptr_`, `_xptr_`, `_beta_` |
| `warcraft-classic` | `classic` | `_classic_` |
| `warcraft-classic-sod` | `sod` | `_classic_era_` |
| `warcraft-vanilla` | `vanilla` | `_classic_era_` |
| `warcraft-classic-fresh` | `fresh` | `_anniversary_` |
| `warcraft-classic-titan-reforged` | `titan` | `_classic_titan_` |

Locale prefixes exist too (`ru.`, `tw.`, `ko.`, ... e.g.
`ru.classic.warcraftlogs.com`).

## 6. WoW log file facts used by the client

- pattern `^WoWCombatLog.*\.txt$`, UTF-8, lives in `<WoW>/_retail_/Logs/`.
- Header lines: any line containing `COMBAT_LOG_VERSION`.
- Line format: `M/D HH:MM:SS.mmm  EVENT,args...` (two spaces between timestamp
  and event). Newer logs carry a full `M/D/YYYY HH:MM:SS.mmm±TZ` timestamp.
- Split heuristic: a gap of more than 4 h between lines starts a new log.
