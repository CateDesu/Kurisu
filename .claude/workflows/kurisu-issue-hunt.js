export const meta = {
  name: 'kurisu-issue-hunt',
  description: 'Exhaustive bug/issue hunt across the Kurisu Tauri+Rust+Svelte codebase with adversarial verification',
  phases: [
    { title: 'Hunt', detail: '20 finders across Rust modules, frontend, security, perf, CI, tests' },
    { title: 'Verify', detail: 'one independent skeptic per finding, prompted to refute' },
    { title: 'Gap sweep', detail: 'completeness critics hunt what round one missed' },
    { title: 'Verify gaps', detail: 'skeptics on the gap-sweep findings' },
    { title: 'Synthesize', detail: 'cross-cutting themes and severity ranking' },
  ],
}

const REPO = '/home/cate/Projects/Kurisu'

const PREAMBLE = [
  'You are auditing the Kurisu codebase at ' + REPO + '.',
  'Kurisu is a Taiga-style AniList anime tracker: Tauri 2 + Rust backend (src-tauri/src) + SvelteKit 5 frontend (src/).',
  'It runs on Linux and Windows, syncs with the AniList GraphQL API, detects playback via MPRIS/GSMTC,',
  'scans a local anime library, parses nyaa RSS torrent feeds, and self-updates from GitHub releases.',
  '',
  'RULES:',
  '- Read the ACTUAL code. Every finding must cite a real file path and a real 1-indexed line number you verified.',
  '- Do NOT run cargo build, cargo clippy, cargo test, or npm build. Another process holds the build locks.',
  '  Use Read and Grep only. You may run trivial non-cargo shell commands (grep, ls, wc).',
  '- Report REAL DEFECTS: wrong behavior, crashes, panics, data corruption, races, leaks, security holes,',
  '  logic errors, unhandled failure modes, incorrect edge-case handling, silent data loss.',
  '- Do NOT report: pure style, formatting, naming preferences, "consider adding a comment", or speculative',
  '  refactors with no behavioral consequence.',
  '- A finding is only worth reporting if you can state a concrete scenario where a user is harmed:',
  '  specific inputs or state, leading to a specific wrong outcome.',
  '- Be exhaustive. Report every real defect you find, no matter how small the blast radius, as long as it is real.',
  '- Prefer precision over volume, but do not self-censor a real defect because it seems minor.',
  '',
  'ALREADY KNOWN / DO NOT RE-REPORT (these were reviewed and are intentional or already fixed at commit a91827f):',
  '- App-setting get/set is allowlisted to close_to_tray + auto_update via APP_SETTING_KEYS (intentional).',
  '- logout() scrubs the token via Db::scrub_setting with DELETE + VACUUM + WAL truncate (already fixed).',
  '- OAuth callback has a bounded header-read loop with a 10s cap and percent-decodes query values (already fixed).',
  '- The updater download is hard-capped at 500 MB (already fixed).',
  '- Release checksum served from the same origin as the artifact: ACCEPTED BY DESIGN, do not report.',
  '- Library scan follows symlinks: ACCEPTED BY DESIGN, do not report.',
  '- EpisodeStepper does not flush pending state on unmount: ACCEPTED BY DESIGN, do not report.',
  '- Notifications URL is split/constructed rather than served whole: ACCEPTED BY DESIGN, do not report.',
  '- Filesystem paths appear in error strings shown to the user: ACCEPTED BY DESIGN, do not report.',
  '- The watched_file table is dormant/unused: ACCEPTED BY DESIGN, do not report.',
  'HOWEVER: if one of those fixes is itself buggy or incomplete, that IS reportable. Judge the code, not the claim.',
  '',
].join('\n')

const FINDINGS_SCHEMA = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string', description: 'One-line statement of the defect' },
          file: { type: 'string', description: 'Repo-relative path, e.g. src-tauri/src/db.rs' },
          line: { type: 'integer', description: '1-indexed line the defect anchors to' },
          severity: { type: 'string', enum: ['critical', 'high', 'medium', 'low'] },
          category: { type: 'string', description: 'kebab-case, e.g. correctness, race, security, panic, leak, data-loss' },
          description: { type: 'string', description: 'What is wrong and why, referencing the actual code' },
          failure_scenario: { type: 'string', description: 'Concrete inputs/state to concrete wrong outcome' },
          evidence: { type: 'string', description: 'The exact code snippet or lines that prove it' },
          fix: { type: 'string', description: 'Concrete suggested fix' },
        },
        required: ['title', 'file', 'line', 'severity', 'category', 'description', 'failure_scenario', 'evidence', 'fix'],
        additionalProperties: false,
      },
    },
  },
  required: ['findings'],
  additionalProperties: false,
}

const VERDICT_SCHEMA = {
  type: 'object',
  properties: {
    refuted: { type: 'boolean', description: 'true if the finding is wrong, already handled, or not a real defect' },
    confidence: { type: 'string', enum: ['certain', 'likely', 'uncertain'] },
    corrected_severity: { type: 'string', enum: ['critical', 'high', 'medium', 'low'] },
    corrected_line: { type: 'integer', description: 'The true 1-indexed line if the finding cited the wrong one; else repeat the original' },
    reasoning: { type: 'string', description: 'What you checked in the code and what you concluded' },
    correction: { type: 'string', description: 'Any correction or narrowing of the claim; empty string if none' },
  },
  required: ['refuted', 'confidence', 'corrected_severity', 'corrected_line', 'reasoning', 'correction'],
  additionalProperties: false,
}

const DIMENSIONS = [
  {
    key: 'anilist-client',
    focus: 'src-tauri/src/anilist.rs (1241 lines)',
    lens: [
      'Audit the AniList GraphQL client end to end. Look for:',
      '- GraphQL query/variable construction errors: wrong field names, wrong types, missing variables, wrong nullability.',
      '- Response deserialization: fields that AniList can legitimately return as null but are parsed as non-optional;',
      '  serde attribute mistakes; silently swallowed parse failures; partial-data responses where "errors" is set but',
      '  "data" is also present.',
      '- HTTP error handling: non-200 statuses, 429 rate limiting (AniList rate-limits aggressively, check for',
      '  Retry-After handling), 401 expired token, network timeouts, missing timeouts entirely.',
      '- Pagination: off-by-one page indexing, infinite loops, unbounded page fetching, missing hasNextPage checks,',
      '  perPage limits.',
      '- Token handling: is the auth header set on every request that needs it, and never on ones that must not have it.',
      '- Any place a GraphQL error is turned into Ok(default) instead of Err, hiding failures from the UI.',
    ].join('\n'),
  },
  {
    key: 'commands-auth',
    focus: 'src-tauri/src/commands.rs lines 1-320 (client id, redirect uri, tracking config, app settings, login_with_token, login_oauth, logout, current_user)',
    lens: [
      'Audit the auth and settings command surface. Look for:',
      '- The OAuth implicit/authorization flow: state parameter generation and VALIDATION (is the returned state actually',
      '  compared?), CSRF, the localhost callback listener (bind address, port collision, port left open, listener not',
      '  shut down, multiple concurrent logins), and what happens if the user never completes the flow.',
      '- Token lifecycle: where the token is stored, whether it is written to disk in plaintext, whether it can leak into',
      '  logs or error strings returned to the frontend, whether logout truly clears in-memory copies as well as the DB.',
      '- Validation of user-supplied client_id and redirect_uri: can a malformed or hostile value be persisted and then',
      '  used to build a URL that is opened in the browser (URL injection / open redirect)?',
      '- The APP_SETTING_KEYS allowlist: is it applied to BOTH get and set, is the failure mode an error or a silent no-op,',
      '  and does every key the frontend actually uses appear in it (grep the frontend for get_app_setting/set_app_setting).',
      '- State mutation races: two commands mutating AppState concurrently, lock held across an await point.',
    ].join('\n'),
  },
  {
    key: 'commands-list',
    focus: 'src-tauri/src/commands.rs lines 418-680 (sync_my_list, local_entries, get_entry, update_entry, increment_episode, set_progress, delete_entry_cmd)',
    lens: [
      'Audit list-mutation correctness and data integrity. This is where user watch data can be corrupted or lost. Look for:',
      '- increment_episode and set_progress: off-by-one, exceeding the total episode count, negative or zero progress,',
      '  progress on an entry with unknown total (null episodes), auto-status transitions (PLANNING to CURRENT to COMPLETED)',
      '  firing wrongly or not at all, rewatch counters.',
      '- Local-vs-remote ordering: is the local DB written before or after the AniList mutation succeeds? What is the state',
      '  if the remote call fails after the local write (or vice versa)? Is there any rollback? Can a failed sync silently',
      '  present stale data as authoritative?',
      '- sync_my_list: does it replace the whole local list or merge? Can a partial/failed fetch wipe local entries?',
      '  Does it handle an empty list, a user with no lists, or duplicate media across custom lists?',
      '- Concurrent increments (user spams the +1 button, or playback detection fires while the user clicks): lost updates,',
      '  double increments, TOCTOU between read-current-progress and write-new-progress.',
      '- delete_entry_cmd: remote delete vs local delete ordering, deleting an entry that does not exist remotely.',
      '- Score handling: scale conversions (POINT_10, POINT_100, POINT_5, POINT_3, POINT_10_DECIMAL), rounding, and whether',
      '  the user score format is respected on write.',
    ].join('\n'),
  },
  {
    key: 'commands-media',
    focus: 'src-tauri/src/commands.rs lines 676-1029 (library folders, scan_library, bind/unbind, RSS feeds, fetch_torrents, mark_torrents_seen, get_user_stats, get_notifications, check_update, install_update)',
    lens: [
      'Audit the library/torrent/stats/update command surface. Look for:',
      '- add_library_folder / add_rss_feed: duplicate entries, no validation that the path exists or the URL is http(s),',
      '  unbounded list growth, removal by exact-string match failing on trailing slashes or case, and whether a hostile',
      '  or file:// URL can be added and then fetched.',
      '- fetch_torrents: what happens when a feed 404s, returns HTML instead of XML, or is huge; is there a size cap and a',
      '  timeout; are multiple feeds fetched serially (slow) and does one bad feed kill all of them.',
      '- is_new computation and rss_seen bookkeeping: can an item be permanently marked seen before the user sees it,',
      '  can the rss_seen table grow without bound, are GUIDs trusted/unique.',
      '- mark_torrents_seen with a huge or empty guid vector: SQL parameter limits, statement building in a loop, missing',
      '  transaction so a partial failure leaves half the rows written.',
      '- scan_library / bind_library_path / unbind: path canonicalization, non-UTF8 paths, missing folders, binding a path',
      '  to a media_id that has no entry, orphaned bindings after a folder is removed.',
      '- check_update / install_update: error paths, and whether install_update can run twice concurrently.',
    ].join('\n'),
  },
  {
    key: 'db-layer',
    focus: 'src-tauri/src/db.rs (519 lines)',
    lens: [
      'Audit the SQLite layer. Look for:',
      '- Schema migrations v1 through v4: is user_version read and written correctly, are migrations idempotent, is each',
      '  step guarded so an interrupted migration cannot leave a half-migrated DB, what happens on a DOWNGRADE (a newer DB',
      '  opened by an older binary after a rollback), and are ALTER TABLE statements safe to re-run.',
      '- The COALESCE upsert that lets lean fetches preserve rich fields: can it preserve a value that SHOULD be cleared',
      '  (e.g. a field legitimately set back to null upstream)? Does it apply to every column that needs it?',
      '- SQL correctness: string-interpolated SQL vs bound parameters (injection), wrong column order in INSERT, missing',
      '  WHERE clauses on UPDATE/DELETE, LIKE patterns with unescaped % or _ from user input.',
      '- Transactions: multi-statement writes without a transaction, so a failure mid-way leaves inconsistent rows.',
      '- Connection handling: is the connection shared behind a mutex, held across awaits, or opened per call; busy_timeout',
      '  and SQLITE_BUSY handling under concurrent access; WAL configuration.',
      '- scrub_setting (DELETE + VACUUM + WAL truncate): is it correct, and can VACUUM fail or block while other work holds',
      '  the connection?',
      '- Indexes: queries that will table-scan a 1280+ entry list on every render.',
    ].join('\n'),
  },
  {
    key: 'recognizer',
    focus: 'src-tauri/src/recognize.rs (505 lines) plus its 11 tests',
    lens: [
      'Audit the filename/title recognizer. This module was rewritten at commit e2235ea to fix wild mismatches, so scrutinize',
      'the NEW logic hard rather than assuming it is correct. Look for:',
      '- strip_ext: the known-extension list, case sensitivity, filenames with no extension, filenames that ARE an extension,',
      '  double extensions.',
      '- Regex correctness: the episode-tail regex, season/batch/version tags, release-group brackets, checksums, resolution',
      '  tokens. Find titles where the regex eats part of the real title or fails to strip a real episode marker.',
      '- The tiered matching (exact > prefix > interior phrase of >=8 chars with multi-word, shorter side >=4 chars):',
      '  construct concrete real-world anime titles that produce a FALSE POSITIVE match or a FALSE NEGATIVE miss.',
      '  Think about short titles (86, K, Bleach), titles that are prefixes of others (Fate/Zero vs Fate/stay night;',
      '  Gintama vs Gintama Season 2), sequels, and romaji-vs-english title variants.',
      '- Unicode: byte-index slicing on a multi-byte string will PANIC. Check every slice, split_at, and index range against',
      '  Japanese titles, accented characters, and full-width punctuation.',
      '- Episode number parsing: episode 0, specials, decimals (12.5), 3-digit episodes (One Piece 1000+), leading zeros,',
      '  ranges (01-12 batches), and the is_new cap against the entry total.',
      '- Performance: regex compiled inside a loop rather than lazily once; O(n*m) matching over a 1280-entry list times a',
      '  75-item feed.',
    ].join('\n'),
  },
  {
    key: 'updater',
    focus: 'src-tauri/src/updater.rs (451 lines)',
    lens: [
      'Audit the in-app self-updater. A bug here BRICKS user installs, so treat every failure path as high stakes. Look for:',
      '- Version comparison: is it semver-aware or string-compared? 1.0.0.10 vs 1.0.0.9, 1.0.0.19 vs 1.0.0.2. The project uses',
      '  4-segment rolling versions. Check for a comparison that would offer a DOWNGRADE or miss an upgrade.',
      '- Download: the 500 MB cap enforcement (is it checked against Content-Length only, which a server can lie about, or',
      '  against bytes actually written?), timeouts, redirect following, partial download detection, disk-full handling.',
      '- SHA-256 sidecar verification: is the hash compared case-insensitively, is the sidecar parsed robustly, and crucially',
      '  is verification done BEFORE the binary is installed/executed. Is there any path where a failed verify still installs?',
      '- The install/swap step: on Windows a running exe cannot be replaced. Check the swap sequence for any window where the',
      '  target exe is absent or truncated (the brick mode). Check the rollback path and the boot-ok marker if present.',
      '- Temp file handling: predictable temp paths (symlink/TOCTOU attack), temp files left behind on failure, writing into',
      '  the install dir without permission checks.',
      '- GitHub API response parsing: no release, draft releases, prereleases, missing asset for this platform, asset name',
      '  matching that could pick the WRONG platform artifact.',
    ].join('\n'),
  },
  {
    key: 'playback',
    focus: 'src-tauri/src/playback.rs (445 lines)',
    lens: [
      'Audit playback detection (MPRIS on Linux, GSMTC on Windows, stub elsewhere). Look for:',
      '- Thread/task lifecycle: is the poller a detached thread or task, does it stop on app exit, can it be started twice,',
      '  does it leak on window close, does it keep the process alive preventing shutdown.',
      '- Blocking calls (mpris DBus, windows GSMTC) executed on the async runtime without spawn_blocking, stalling the whole',
      '  Tokio worker pool and freezing unrelated commands.',
      '- Panic safety: a panic in the polling thread silently kills detection for the rest of the session with no user-visible',
      '  error. Check for unwrap/expect on DBus or Windows API results.',
      '- The cfg gating: does the non-Linux non-Windows stub compile and have the same signature; are there cfg(windows) blocks',
      '  that would not compile (this is a Linux dev machine, so Windows code may never have been compiled here).',
      '- Detection logic: player title parsing, distinguishing paused vs playing, the watched-threshold/percentage rule for',
      '  when an episode counts as watched, seeking backwards, restarting the same file, and whether the same episode can be',
      '  auto-incremented twice.',
      '- Detecting a file that is not anime at all and updating a random entry.',
    ].join('\n'),
  },
  {
    key: 'app-setup',
    focus: 'src-tauri/src/lib.rs (332), src-tauri/src/main.rs (6), src-tauri/src/library.rs (202)',
    lens: [
      'Audit app bootstrap, state, tray, and the library scanner. Look for:',
      '- setup(): ordering problems (state used before init), failures swallowed at startup leaving a half-initialized app,',
      '  panics during setup that crash before any window is shown, DB init failure handling.',
      '- Tray icon and close-to-tray: window close vs app exit semantics, the close_to_tray setting being read once and cached,',
      '  the app becoming unreachable (no window, no tray), double-registration of event handlers.',
      '- Background tasks spawned at startup: unbounded intervals, tasks that never stop, tasks that hold state locks,',
      '  tasks that hammer AniList or the RSS feeds on a tight loop.',
      '- library.rs directory walk: recursion depth (no limit = stack/time blowup on a deep tree), cycles, permission-denied',
      '  entries aborting the whole scan instead of being skipped, non-UTF8 filenames, hidden/system dirs, huge directories,',
      '  the extension filter, and whether the scan blocks the UI thread.',
      '- Path handling that differs on Windows (backslashes, UNC paths, drive letters, case-insensitive comparison).',
    ].join('\n'),
  },
  {
    key: 'parsing',
    focus: 'src-tauri/src/rss.rs (264), src-tauri/src/models.rs (335)',
    lens: [
      'Audit XML parsing and the shared data models. Look for:',
      '- quick-xml usage: XXE / external entity handling, billion-laughs entity expansion, unbounded buffer growth on a',
      '  malformed or hostile feed, CDATA handling, nested/unclosed tags, encoding declarations, BOM, non-UTF8 bytes.',
      '- Fields read from the feed that are required by the parser but optional in real RSS (missing guid, missing pubDate,',
      '  missing title) causing a whole feed to be dropped instead of one item skipped.',
      '- Date parsing: RFC 2822 vs RFC 3339, timezone handling, and what happens when pubDate is garbage.',
      '- nyaa-specific fields (seeders, leechers, size, category) parsed as integers without handling empty strings or commas.',
      '- models.rs: serde defaults and Option-ness matching what AniList ACTUALLY returns; integer types too small for real',
      '  values (i32 vs i64 for ids/timestamps); enums that will fail to deserialize on an unknown variant the API adds later;',
      '  fields renamed with serde(rename) that do not match the GraphQL query aliases.',
      '- Any Deserialize impl where a single unexpected field kills the whole response.',
    ].join('\n'),
  },
  {
    key: 'svelte-runes',
    focus: 'all of src/**/*.svelte and src/lib/*.svelte.ts (library.svelte.ts, auth.svelte.ts, now.svelte.ts)',
    lens: [
      'Audit Svelte 5 runes correctness. Look for:',
      '- $state used on a value that is then destructured or captured, losing reactivity.',
      '- $derived vs $derived.by misuse; $derived depending on a value mutated inside an $effect (infinite loop or lost update).',
      '- $effect with missing cleanup: intervals, timeouts, event listeners, and Tauri event listen() subscriptions that are',
      '  never unsubscribed on unmount, leaking across navigations.',
      '- $effect that writes state it also reads, causing an update loop.',
      '- Async work inside $effect where the component can unmount before the promise resolves, then setting state on a dead',
      '  component or overwriting newer data with a stale response (out-of-order responses).',
      '- Props declared with $props() but mutated locally; $bindable misuse.',
      '- Keyed each blocks missing a key, causing wrong-row updates in the list/library/torrent tables.',
      '- Module-level state in .svelte.ts shared across routes that is never reset on logout.',
      '- Race between onMount data loading and route navigation.',
    ].join('\n'),
  },
  {
    key: 'contract-drift',
    focus: 'src/lib/types.ts (399), src/lib/api.ts (100) vs src-tauri/src/models.rs and the #[tauri::command] signatures in commands.rs',
    lens: [
      'Audit the frontend/backend contract. Be systematic: enumerate EVERY #[tauri::command] in commands.rs and every invoke',
      'call in the frontend, then diff them. Look for:',
      '- invoke() calls with a command name that does not exist, or a typo.',
      '- Argument NAME mismatches: Tauri converts snake_case Rust params to camelCase on the JS side. Find every call where',
      '  the JS object key does not match what Rust expects (this fails at runtime, silently, as a rejected promise).',
      '- Argument TYPE mismatches: number vs string, null passed where Rust expects a non-Option, missing optional args.',
      '- types.ts interfaces that disagree with the serde representation in models.rs: missing fields, extra fields, a field',
      '  typed non-optional in TS that is Option<T> in Rust (undefined at runtime, then .toFixed() or .length crashes the UI),',
      '  enum string values that differ, i64 fields typed as number (JS precision) where it matters.',
      '- Commands registered in the invoke_handler in lib.rs vs commands actually defined: a command defined but never',
      '  registered fails at runtime.',
      '- Rejected invokes whose error string is displayed raw, or swallowed entirely.',
    ].join('\n'),
  },
  {
    key: 'ui-state-races',
    focus: 'src/lib/EditEntry.svelte, EpisodeStepper.svelte, Tracking.svelte, LinkAnime.svelte, Select.svelte, ScoreInput.svelte, Updater.svelte, library.svelte.ts and the routes that use them',
    lens: [
      'Audit UI-level state correctness and races. Look for:',
      '- Double-submit: a save/increment button that is not disabled during the in-flight request, so two mutations race and',
      '  the second overwrites or double-applies.',
      '- Optimistic updates that are never rolled back when the backend call fails, leaving the UI showing data the server',
      '  rejected until a manual refresh.',
      '- Debounced/throttled writes (the episode stepper, the score input) where the pending value is lost on navigation,',
      '  or where the debounce fires with a stale captured value.',
      '- Modal/dialog state: closing while a request is in flight, reopening with stale form values, the form not resetting',
      '  between different anime.',
      '- Search-as-you-type: out-of-order responses rendering an older query result over a newer one; no abort of the previous',
      '  request.',
      '- List refresh after a mutation: does the local list actually update, or does it require a full re-sync; are two',
      '  sources of truth (a store plus a local copy) allowed to diverge.',
      '- Score input validation: values outside the score-format range, decimals where only integers are allowed, empty input.',
      '- Loading/error/empty states missing entirely, so a failure looks like an empty library.',
    ].join('\n'),
  },
  {
    key: 'frontend-security',
    focus: 'all of src/, plus the CSP in src-tauri/tauri.conf.json and the permission list in src-tauri/capabilities/default.json',
    lens: [
      'Audit frontend-side security. Look for:',
      '- {@html ...} anywhere: AniList descriptions and character/staff bios contain user-submitted HTML. If any of that is',
      '  rendered with @html, that is stored XSS inside a webview that has IPC access to the Rust backend. Check how',
      '  descriptions, bios, and torrent titles are rendered.',
      '- Untrusted URLs passed to the opener plugin (open_url / open_path / reveal_item_in_dir): torrent links and magnet',
      '  URIs come from a remote RSS feed. Is the scheme validated? opener:allow-open-path plus a remote-controlled path is',
      '  arbitrary file/program launching.',
      '- The CSP: img-src allows https://*.anilist.co, but check whether image URLs from the API are actually constrained to',
      '  that, whether connect-src is absent (defaulting to default-src self, which would block nothing extra but check',
      '  whether the frontend does any direct fetch that must be proxied through Rust), and whether unsafe-inline in style-src',
      '  is exploitable given the rest.',
      '- The capability permission list: is anything granted that the app does not need (allow-open-path, reveal-item-in-dir),',
      '  and can the renderer reach it with attacker-influenced arguments.',
      '- Any place a token, client id, or redirect uri is put into the DOM, a URL, a log, or window.location.',
    ].join('\n'),
  },
  {
    key: 'concurrency-panics',
    focus: 'all of src-tauri/src/*.rs, cross-cutting',
    lens: [
      'Audit concurrency, async correctness, and panic safety across the whole Rust backend. There are 54 unwrap/expect/panic',
      'sites; triage them. Look for:',
      '- A parking_lot Mutex/RwLock guard held ACROSS an .await point. parking_lot guards are not Send-safe across awaits and',
      '  this deadlocks or blocks the runtime. Find every one.',
      '- Lock-ordering inversions between two locks taken in different orders in different functions (classic deadlock).',
      '- Blocking I/O (std::fs, rusqlite, std::net, DBus) called from an async fn without spawn_blocking, stalling Tokio',
      '  workers. With a multi-thread runtime this shows up as intermittent UI freezes under load.',
      '- unwrap()/expect() on anything that can fail at runtime: lock poisoning, missing state, parse of remote data, slicing,',
      '  integer conversion, time operations (SystemTime before UNIX_EPOCH), env vars, path components. A panic inside a Tauri',
      '  command surfaces as a broken promise or kills the app.',
      '- Integer arithmetic that can overflow or underflow (subtracting progress, episode counts, durations, byte counts).',
      '  In release mode this wraps silently.',
      '- Reentrancy: a command that can be invoked again while the first is still running, on shared mutable state.',
      '- tokio::spawn tasks whose JoinHandle is dropped, so a panic inside is completely silent.',
    ].join('\n'),
  },
  {
    key: 'security-audit',
    focus: 'whole repo, security lens',
    lens: [
      'Do a focused security review of the whole application. Threat model: a malicious RSS feed, a compromised or hostile',
      'AniList response, a malicious file in the scanned library, a local attacker on the machine, and a network attacker',
      'on localhost. Look for:',
      '- Secret handling: the AniList OAuth token at rest and in memory, in logs (env_logger + log:: calls — grep for any',
      '  log line that could include the token or the Authorization header), in error strings returned to the frontend, and',
      '  in the DB file permissions.',
      '- The localhost OAuth callback listener: bound to 127.0.0.1 or 0.0.0.0? Any other local process can connect to it.',
      '  Is the state parameter validated? Can a malicious local page hit the callback and inject a token?',
      '- TLS: reqwest is configured with rustls; check nothing disables certificate verification, and that every remote URL',
      '  is https (grep for http://).',
      '- Path traversal: any path built from remote data (torrent titles, AniList titles) used for filesystem access.',
      '- Command/argument injection through the opener plugin.',
      '- The updater as a supply-chain surface: where the release URL comes from, whether it can be redirected, and whether',
      '  a downgrade attack is possible.',
      '- Denial of service on the app itself: a feed or API response that causes unbounded memory or CPU use.',
      'Report real, reachable issues; do not report theoretical hardening with no attacker path.',
    ].join('\n'),
  },
  {
    key: 'perf-resources',
    focus: 'whole repo, performance and resource lens',
    lens: [
      'Audit performance and resource use. The real user has a 1280-entry AniList list and a live nyaa feed, so scale matters.',
      'Look for:',
      '- N+1 queries or a per-row DB round trip inside a loop over the whole list (sync_my_list, scan_library, fetch_torrents).',
      '- Missing bulk/transaction wrapping so 1280 inserts each get their own fsync (this is a multi-second stall).',
      '- Recognizer matching that is O(list * feed) with expensive per-comparison work (allocation, lowercasing, regex) inside',
      '  the inner loop rather than precomputed once.',
      '- Regex::new called on every invocation instead of once (no lazy static / OnceLock).',
      '- Unbounded growth: the rss_seen table, cached media rows, notification history, log files, in-memory vectors that are',
      '  appended to but never trimmed.',
      '- Cloning large Vec<ListEntry> / Vec<Media> repeatedly to cross the IPC boundary or to satisfy the borrow checker.',
      '- The whole list being serialized to the frontend on every small mutation.',
      '- Frontend: rendering 1280 rows without virtualization, expensive $derived recomputation over the full list on every',
      '  keystroke, images loaded without lazy loading, sort/filter re-running on unrelated state changes.',
      '- Polling intervals that are too aggressive (playback detection, RSS refresh, update checks).',
    ].join('\n'),
  },
  {
    key: 'ci-release',
    focus: '.github/workflows/windows-build.yml, .github/workflows/discord-notify.yml, src-tauri/tauri.conf.json, src-tauri/Cargo.toml, package.json, .gitignore',
    lens: [
      'Audit CI, release, and packaging. Every push to main cuts a rolling GitHub release consumed by the in-app updater, so a',
      'workflow bug ships broken binaries or bricks the updater. Look for:',
      '- The draft-gating logic: can the release ever be published with only one platform artifact; does the publish job run',
      '  on failure or cancellation; is the prune job able to delete a release that the updater is currently serving; can prune',
      '  delete the WRONG release (tag pattern too broad); what happens on concurrent pushes to main (two runs racing on the',
      '  same tag).',
      '- Version consistency: Cargo.toml says 1.0.0, tauri.conf.json says 1.0.0, package.json says 1.0.0, but releases are',
      '  1.0.0.19. Where does the 4th segment come from, and can the version the updater compares against disagree with the',
      '  version baked into the binary? That would cause an infinite update loop or a missed update.',
      '- Checksum/sidecar generation matching exactly what updater.rs expects to parse (filename, format, case).',
      '- Asset naming vs the platform-matching logic in updater.rs.',
      '- Secrets: any secret echoed, passed via a command line visible in logs, or exposed to a pull_request trigger from a fork.',
      '- Permissions blocks (contents: write) scoped too broadly; unpinned third-party actions (supply-chain).',
      '- .gitignore: is project-status.md (internal notes) actually excluded; could any secret or local artifact be committed.',
      '- Cargo.toml: dependency features that will not compile on the other platform, missing panic/opt profile settings,',
      '  the crate-type list being wrong for a Tauri app.',
    ].join('\n'),
  },
  {
    key: 'test-audit',
    focus: 'every #[test] and #[tokio::test] in src-tauri/src (25 total: recognize 11, commands 3, db 2, rss 2, anilist 2, updater 2, library 1, playback 1, models 1)',
    lens: [
      'Audit the test suite itself, and the gaps it leaves. Look for:',
      '- Tests that assert nothing meaningful, assert on their own input, or would pass even if the function under test were',
      '  replaced with a stub. These create FALSE CONFIDENCE and are worth reporting.',
      '- Tests that pin in WRONG behavior: an assertion that encodes a bug as expected.',
      '- The models.rs / playback.rs drift tests: do they actually detect a field added to Rust but missing from types.ts,',
      '  or are they cosmetic? Read them and judge whether they can fail.',
      '- The 11 recognizer tests: do they cover the specific mismatch classes fixed at commit e2235ea (the "No.6"/"D.Gray-man"',
      '  dot truncation, the "86" numeric-suffix eating, the degenerate-norm substring match)? Name the regression cases that',
      '  are NOT covered.',
      '- Critical untested paths, ranked: which of the 41 Tauri commands, which failure branches, which migration steps have',
      '  zero coverage and the highest blast radius if wrong.',
      '- Tests that depend on the environment (network, real filesystem paths, the user real DB, wall-clock time, ordering)',
      '  and will flake or, worse, mutate real user data when run.',
      'Report each significant gap or bad test as its own finding with a concrete failure scenario.',
    ].join('\n'),
  },
  {
    key: 'failure-modes',
    focus: 'whole stack, failure-path lens',
    lens: [
      'Audit what happens when things go WRONG, end to end. Trace each scenario through the actual code (Rust command,',
      'IPC boundary, Svelte handler, rendered UI) and report every place it is handled badly:',
      '- No network at all: app launch, sync, search, detail page, images, torrent fetch, update check. Does the app show a',
      '  clear error, hang forever with a spinner, or silently show an empty list that looks like real data?',
      '- Token expired or revoked (AniList returns 401): is it detected, is the user prompted to re-login, or does every',
      '  action just fail with a cryptic string? Does a 401 wrongly clear local data?',
      '- AniList returns a GraphQL error with HTTP 200 (this is its normal error mode). Is that detected at all?',
      '- Rate limited (429): does the app retry, back off, or hammer?',
      '- The DB file is missing, read-only, corrupt, or locked by a second instance of the app. Is a second instance even',
      '  prevented? Two instances sharing one SQLite file with different in-memory state is a data-loss scenario.',
      '- A library folder is on an unmounted drive or a dead network mount (scan hangs).',
      '- First-run empty state: no token, no folders, no feeds, empty list. Any unwrap on a missing config, any division by',
      '  zero in the stats page, any .toFixed on undefined.',
      '- Media with null fields: no episodes count, no cover image, no description, no airing schedule, TBA titles.',
      'Each distinct badly-handled scenario is its own finding.',
    ].join('\n'),
  },
]

function findPrompt(d) {
  return [
    PREAMBLE,
    'YOUR ASSIGNMENT: ' + d.key,
    'PRIMARY FOCUS: ' + d.focus,
    '',
    d.lens,
    '',
    'You may read any other file in the repo for context, but your findings should center on your focus area.',
    'Work systematically: read the full focus file(s) first, then hunt. Do not stop at the first few issues.',
    'Return every real defect you found, most severe first.',
  ].join('\n')
}

function verifyPrompt(f, dimKey) {
  return [
    'You are an adversarial verifier auditing a code-review finding about the Kurisu codebase at ' + REPO + '.',
    'Kurisu is a Tauri 2 + Rust + SvelteKit 5 AniList anime tracker.',
    '',
    'YOUR JOB IS TO REFUTE THIS FINDING. Assume it is wrong until the code proves otherwise.',
    '',
    'CLAIM: ' + f.title,
    'LOCATION: ' + f.file + ':' + f.line,
    'SEVERITY CLAIMED: ' + f.severity,
    'DESCRIPTION: ' + f.description,
    'FAILURE SCENARIO CLAIMED: ' + f.failure_scenario,
    'EVIDENCE CITED: ' + f.evidence,
    '',
    'Open the file. Read the cited lines AND enough surrounding context to judge fairly. Then check, specifically:',
    '1. Does the cited code actually exist and say what the finding claims? (Line numbers are often wrong; if the code is',
    '   there but at a different line, that is NOT a refutation — set corrected_line and judge the substance.)',
    '2. Is the defect already handled somewhere the finder did not look: a caller-side guard, an early return, a type',
    '   invariant, a serde default, a DB constraint, a Svelte reactivity guarantee, a Tauri framework behavior?',
    '3. Is the failure scenario actually REACHABLE by a real user or a real input? Trace the call path. If nothing can reach',
    '   that state, refute it.',
    '4. Is it merely stylistic, or a hypothetical with no concrete harm? Refute it.',
    '5. Is it on the ACCEPTED-BY-DESIGN list (same-origin release checksum, symlink-following library scan, stepper',
    '   flush-on-unmount, notifications URL split, paths in error strings, dormant watched_file table) or already fixed at',
    '   commit a91827f (app-setting allowlist, token scrub on logout, OAuth callback header-read cap and percent-decode,',
    '   updater 500 MB cap)? If so, refute it — UNLESS the finding identifies a genuine bug IN that fix.',
    '',
    'Do NOT run cargo or npm build commands; another process holds the build locks. Read and grep only.',
    '',
    'Set refuted=true if the finding is wrong, unreachable, already handled, or not a real defect.',
    'Set refuted=false ONLY if you independently confirmed a real defect with real user impact.',
    'If you confirm it but the severity is inflated or deflated, set corrected_severity to what it should really be.',
    'If the claim is partially right (right bug, wrong explanation; or narrower than claimed), set refuted=false and put',
    'the precise correction in the correction field.',
    'When genuinely uncertain after real investigation, set confidence=uncertain and refuted=false rather than guessing.',
  ].join('\n')
}

// ---------------- Phase 1 + 2: hunt, and verify each finding as its dimension lands ----------------

phase('Hunt')
log('Hunting across ' + DIMENSIONS.length + ' dimensions of the Kurisu codebase')

const hunted = await pipeline(
  DIMENSIONS,
  (d) => agent(findPrompt(d), { label: 'hunt:' + d.key, phase: 'Hunt', schema: FINDINGS_SCHEMA }),
  async (found, d) => {
    const list = (found && found.findings) || []
    if (!list.length) {
      log('hunt:' + d.key + ' found nothing')
      return []
    }
    log('hunt:' + d.key + ' raised ' + list.length + ' findings, verifying')
    const checked = await parallel(
      list.map((f, i) => () =>
        agent(verifyPrompt(f, d.key), {
          label: 'verify:' + d.key + ':' + (i + 1),
          phase: 'Verify',
          schema: VERDICT_SCHEMA,
        }).then((v) => ({ dim: d.key, round: 1, finding: f, verdict: v }))
      )
    )
    return checked.filter(Boolean)
  }
)

const round1 = hunted.filter(Boolean).flat().filter(Boolean)
const survived1 = round1.filter((r) => r.verdict && r.verdict.refuted === false)
const killed1 = round1.filter((r) => r.verdict && r.verdict.refuted === true)
log('Round 1: ' + round1.length + ' raised, ' + survived1.length + ' confirmed, ' + killed1.length + ' refuted')

// ---------------- Phase 3: completeness critics hunt what round one missed ----------------

phase('Gap sweep')

const covered = survived1
  .map((r) => '- [' + r.dim + '] ' + r.finding.file + ':' + (r.verdict.corrected_line || r.finding.line) + ' — ' + r.finding.title)
  .join('\n')

const GAP_LENSES = [
  {
    key: 'gap-untouched-code',
    prompt: [
      'Round one of this audit covered 20 dimensions but coverage is never uniform. Your job is to find defects in the code',
      'that round one did NOT look at closely. Enumerate the repo (src-tauri/src/*.rs, src/**/*), compare against the',
      'confirmed-findings list below, and go hunt in the functions, branches, and files that are absent from it.',
      'Pay special attention to: error branches nobody traced, helper functions, the middle of long files that a single',
      'reader likely skimmed, and any module where round one produced suspiciously few findings.',
    ].join('\n'),
  },
  {
    key: 'gap-cross-module',
    prompt: [
      'Round one audited modules mostly in isolation. Your job is to find defects that only exist at the SEAMS between',
      'modules, invisible to any single-module reader. Trace complete end-to-end flows through the real code:',
      '(a) user clicks +1 episode -> Svelte handler -> invoke -> commands.rs -> anilist.rs mutation -> db.rs write -> UI refresh;',
      '(b) playback detected -> playback.rs -> recognize.rs match -> commands.rs progress update -> AniList -> UI;',
      '(c) RSS refresh -> rss.rs parse -> recognize.rs match against the list -> is_new -> rss_seen -> torrents page;',
      '(d) app launch -> lib.rs setup -> db migration -> token load -> sync_my_list -> first render;',
      '(e) update check -> updater.rs -> download -> verify -> install -> relaunch.',
      'Look for state that one module assumes another maintains, invariants enforced on one side only, data transformed',
      'twice or not at all, units/ids/indices that change meaning across a boundary, and error information lost in translation.',
    ].join('\n'),
  },
  {
    key: 'gap-adversarial-inputs',
    prompt: [
      'Round one reasoned about code. Your job is to break it with INPUTS. Construct concrete hostile or merely weird inputs',
      'and trace each one through the real code until it produces a wrong result, a panic, or a hang. Cover at minimum:',
      '- Anime titles: unicode (Japanese, accents, full-width), extremely long, empty, whitespace-only, titles that are pure',
      '  digits, titles containing regex metacharacters or SQL wildcards, duplicate titles across different media ids.',
      '- Filenames: no extension, nested brackets, multiple episode-like numbers, 4-digit episodes, decimals, unicode,',
      '  path separators inside the name, names longer than the OS limit.',
      '- RSS: malformed XML, an item with no guid, a 50 MB feed, HTML served instead of XML, entity expansion, wrong encoding.',
      '- AniList responses: nulls in every nullable field, an unknown enum variant, a 200 with a GraphQL errors array,',
      '  an id that exceeds 2^53 in JS, an empty list, a list with 5000 entries.',
      '- Numbers: progress 0, progress > episodes, negative, episodes null, score at both ends of every score format.',
      'Report only inputs you traced to a specific line that mishandles them.',
    ].join('\n'),
  },
  {
    key: 'gap-severity-hunter',
    prompt: [
      'Round one produced mostly medium/low findings in some areas. Your job is to hunt SPECIFICALLY for the highest-impact',
      'classes of defect, the ones that lose user data, corrupt state, or brick the install. Nothing else. Search the whole',
      'repo for:',
      '- Any path that can write WRONG watch progress or a WRONG score to the user real AniList account (this is destructive',
      '  and remote; the user 1280-entry list is real data).',
      '- Any path that can DELETE or overwrite local DB rows or the DB file, including a failed migration, a failed sync,',
      '  a second app instance, or a VACUUM/scrub that runs at the wrong moment.',
      '- Any path that can leave the installed application unable to start (updater swap, config write, DB schema).',
      '- Any path that can leak the AniList OAuth token off the machine or into a file the user would share (a log, a crash',
      '  dump, an error message they paste into an issue).',
      '- Any unbounded loop, retry, or recursion that could hammer the AniList API and get the user account rate-limited or',
      '  banned.',
      'Trace each candidate to certainty in the real code before reporting. Quality over quantity here.',
    ].join('\n'),
  },
]

const gapHunted = await pipeline(
  GAP_LENSES,
  (g) =>
    agent(
      [
        PREAMBLE,
        'YOUR ASSIGNMENT: ' + g.key + ' (second-round completeness sweep)',
        '',
        g.prompt,
        '',
        'ALREADY CONFIRMED IN ROUND ONE — do NOT re-report these, find what they missed:',
        covered || '(round one confirmed nothing)',
      ].join('\n'),
      { label: 'gap:' + g.key, phase: 'Gap sweep', schema: FINDINGS_SCHEMA }
    ),
  async (found, g) => {
    const list = (found && found.findings) || []
    if (!list.length) return []
    log('gap:' + g.key + ' raised ' + list.length + ' findings, verifying')
    const checked = await parallel(
      list.map((f, i) => () =>
        agent(verifyPrompt(f, g.key), {
          label: 'verify2:' + g.key + ':' + (i + 1),
          phase: 'Verify gaps',
          schema: VERDICT_SCHEMA,
        }).then((v) => ({ dim: g.key, round: 2, finding: f, verdict: v }))
      )
    )
    return checked.filter(Boolean)
  }
)

const round2 = gapHunted.filter(Boolean).flat().filter(Boolean)
const survived2 = round2.filter((r) => r.verdict && r.verdict.refuted === false)
log('Round 2: ' + round2.length + ' raised, ' + survived2.length + ' confirmed')

// ---------------- Phase 5: synthesis ----------------

phase('Synthesize')

const allConfirmed = survived1.concat(survived2)

const synthesis = await agent(
  [
    'You are synthesizing the results of an exhaustive multi-agent audit of the Kurisu codebase at ' + REPO + '.',
    'Kurisu is a Tauri 2 + Rust + SvelteKit 5 AniList anime tracker.',
    '',
    'Below are the findings that survived independent adversarial verification. Analyze them AS A SET.',
    '',
    JSON.stringify(
      allConfirmed.map((r) => ({
        dim: r.dim,
        title: r.finding.title,
        file: r.finding.file,
        line: r.verdict.corrected_line || r.finding.line,
        severity: r.verdict.corrected_severity || r.finding.severity,
        category: r.finding.category,
        failure_scenario: r.finding.failure_scenario,
      })),
      null,
      1
    ),
    '',
    'Read enough of the actual code to ground your analysis. Do NOT run cargo or npm; read and grep only. Produce:',
    '1. THEMES: the 3-6 recurring root causes behind these findings (e.g. "local DB written before the remote mutation is',
    '   confirmed" or "remote data parsed with non-optional types"). Name the specific findings under each theme.',
    '2. CLUSTERS: findings that are really the same underlying bug reported from different angles, and should be fixed once.',
    '3. FIX ORDER: a concrete ordering, accounting for which fixes are prerequisites for others and which are one-line wins',
    '   versus structural work. Be specific about what to change first and why.',
    '4. SYSTEMIC RISKS: what these findings collectively say about where the codebase is most fragile, and which single',
    '   subsystem most deserves a dedicated hardening pass.',
    'Be concrete and reference file:line. Return well-structured prose with headings. No preamble, no filler.',
  ].join('\n'),
  { label: 'synthesize', phase: 'Synthesize' }
)

log('Audit complete: ' + allConfirmed.length + ' confirmed findings')

return {
  stats: {
    raised: round1.length + round2.length,
    confirmed: allConfirmed.length,
    refuted: round1.length + round2.length - allConfirmed.length,
  },
  confirmed: allConfirmed.map((r) => ({
    dim: r.dim,
    round: r.round,
    severity: r.verdict.corrected_severity || r.finding.severity,
    confidence: r.verdict.confidence,
    category: r.finding.category,
    title: r.finding.title,
    file: r.finding.file,
    line: r.verdict.corrected_line || r.finding.line,
    description: r.finding.description,
    failure_scenario: r.finding.failure_scenario,
    evidence: r.finding.evidence,
    fix: r.finding.fix,
    verifier_note: r.verdict.correction,
    verifier_reasoning: r.verdict.reasoning,
  })),
  refuted: round1
    .concat(round2)
    .filter((r) => r.verdict && r.verdict.refuted === true)
    .map((r) => ({
      dim: r.dim,
      title: r.finding.title,
      file: r.finding.file,
      line: r.finding.line,
      why_refuted: r.verdict.reasoning,
    })),
  synthesis: synthesis,
}
