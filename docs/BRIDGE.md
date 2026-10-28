# Phone bridge (`sesync bridge`)

A WebSocket server that lets the Orca-derived Android app (agent-hub) pair with this machine and work
with its sesync conversations: list them, read each transcript, open its herdr pane live, type prompts, answer approvals, send images, start sessions and receive notifications. Phase 2 adds the chat, approval, image, new-session, notification and usage methods; the design is in `~/projects/agent-hub-notes/native-host-contract.md`.
Phase 1 (agent-hub, native host screens, option A). The app renders the host screens itself; the bridge
serves data RPCs only. Only the `sesync bridge` subcommand runs any of this. The daemon, the TUI and the
team client never load it.

Orca is MIT-licensed (stablyai/orca). Its protocol is read from the source at the commit in
`~/projects/orca-phone` (`mobile/src/transport/*`, `mobile/src/session/*`, `src/shared/rpc-contract/*`,
`src/main/runtime/rpc/methods/terminal/*`). The shared contract with the app is
`~/projects/agent-hub-notes/native-host-contract.md`.

## Commands

| Command | What it does |
|---|---|
| `sesync bridge pair [--endpoint ws://HOST:PORT] [--label NAME]` | Pairs a new device. Prints the pairing offer (v2 JSON), the `agenthub://pair?code=` URL and a terminal QR. The device token is in that output and nowhere else. |
| `sesync bridge serve [--listen ADDR:PORT] [--allow CIDR,…] [--herdr-socket PATH] [--ntfy-url URL] [--ntfy-topic NAME] [--ntfy-token-command CMD] [--no-ntfy] [--usage-every SECONDS] [--omp PATH] [--no-usage]` | Serves in the foreground until Ctrl-C. Defaults: `0.0.0.0:6768`, allowed networks `10.10.20.0/24,192.168.1.0/24,127.0.0.0/8`, herdr socket `$HERDR_SOCKET_PATH` else `$HOME/.config/herdr/herdr.sock`, ntfy `http://192.168.100.178:30181` topic `agents`, plan usage every 600 s from `omp`. |
| `sesync bridge devices` | Lists paired devices: id, label, creation time. Never prints tokens. |
| `sesync bridge revoke <device-id>` | Removes a device. Its token is refused on the next frame, with no restart. |

`--endpoint` defaults to `ws://10.10.20.3:6768`, this machine's WireGuard mesh address. The phone
must reach that address (WireGuard) or the LAN address the offer names. Change `--endpoint` for
another route. Nothing here opens a firewall port: the host's firewalld zone decides what is reachable.

`serve` takes the sesync-lock binary and its claim directory from the same environment the daemon uses
(`SESYNC_LOCK`, else the binary beside `sesync`; `SESYNC_LOCK_DIR`, else the stored locks directory). If
the lock binary is not installed, prompts are sent without a claim, as the daemon's own writes are.

`serve` also runs the phase-2 background work. The ntfy token command runs once at startup (default `sudo -n systemd-creds decrypt --name=agenthub-ntfy-token /opt/homelab-creds/agenthub-ntfy-token.cred -`, 20 s limit). Its output is the publish token and is never logged; a failure leaves push off and says why. `--no-ntfy` turns push off. Plan usage runs `<omp> usage --json` every `--usage-every` seconds (no model call); `--no-usage` turns it off. `SESYNC_USAGE_FIXTURE=<file>` makes the refresh read that file instead of running omp; the e2e backend uses it. The notification watcher follows herdr's agent-status events (see Notifications).

Exit codes: 0 success, 2 usage, 1 failure (as in `sesync team`).

## Files

Under `$SESYNC_CONFIG_DIR/bridge/` (default `~/.config/sesync/bridge/`). The directory is `0700`.

- `server.key`: `0600`. The bridge's X25519 secret, base64. Created on first use, never replaced
  silently (a new key would strand every paired phone). The public key is derived from it.
- `devices.json`: `0600`. Per device: `id`, `label`, `created` (unix seconds), `token_sha256`.
  The token itself is never stored: it is 256 random bits, and only its SHA-256 is kept.
- `pins.json`: `0600`. The pinned conversations (`worktree.set`) as `{"pinned":["<worktree id>", …]}`. A
  damaged file fails the next pin write rather than being replaced.

Writes are atomic (temp file, then rename) and created owner-only from the start.

## Pairing offer v2

The offer is the JSON below, encoded as unpadded base64url and carried as `agenthub://pair?code=…`
(the same encoding Orca's `encodePairingOffer` produces under `orca://`). The QR code carries the same
URL. The phone app registers the `agenthub` scheme and still reads `orca://pair?code=` links printed
before this change; `sesync bridge` prints only `agenthub://`.

```json
{"v":2,"endpoint":"ws://10.10.20.3:6768","deviceToken":"<43 chars>","publicKeyB64":"<44 chars>"}
```

- `publicKeyB64` pins the bridge's identity; it is the only server key the phone is given.
- There is no `relay` block and no `scope`: direct offers only.

## Wire protocol

Frames are WebSocket messages. A plaintext frame is JSON. An encrypted text frame is
`base64(nonce ‖ box)`, where `box` is NaCl's `crypto_box` output under
`shared = nacl.box.before(serverPublic, clientSecret)` with a random 24-byte nonce. An encrypted
**binary** frame is the same `nonce ‖ box` without base64: the layout Orca's legacy channel reads with
`openLegacyBinary`, which the direct transport routes to its terminal stream router.

1. Client → `{"type":"e2ee_hello","publicKeyB64":…}` (plaintext). Bridge → `{"type":"e2ee_ready"}`.
2. Client → encrypted `{"type":"e2ee_auth","deviceToken":…}`. Bridge → encrypted
   `{"type":"e2ee_authenticated"}`, or encrypted `{"type":"e2ee_error","error":{"code":"unauthorized"}}`
   and close `4001`.
3. Client → encrypted `{"id","deviceToken","method","params"}`. Bridge → encrypted
   `{"id","ok":true,"result",…}` or `{"id","ok":false,"error":{"code","message"}}`. Each request
   carries the token; it is checked on every frame.
4. A request that opens a stream gets encrypted `{"id","ok":true,"streaming":true,"result":{…}}`
   replies, all with the request's `id`, until `result:{"type":"end"}` or an unsubscribe.

Served methods:

| Method | Result |
|---|---|
| `status.get` | `{runtimeId, appVersion, protocolVersion:3, minCompatibleMobileVersion:2, capabilities:[], floatingWorkspaceEnabled:false, hostPlatform, machineName}` |
| `worktree.ps` | `{worktrees:[…], totalCount, truncated:false}`: one row per sesync conversation, newest first. `status` and `agents` come from the herdr panes running it (see Mapping). Without herdr the rows still load, as not open. `isPinned` is the conversation's pin. |
| `worktree.set` `{worktree, isPinned}` | `{worktree, isPinned}`: pins a conversation. The pin is kept in `pins.json` under the bridge's state directory, never in sesync's store. Any other non-null field is refused as `bad_request`. |
| `repo.list` | `{repos:[…]}`: the conversations' project directories, for grouping. Ids match the rows' `repoId`. |
| `session.tabs.list` `{worktree:"id:<wt>"}` | `{worktree, publicationEpoch, snapshotVersion, tabs:[…], activeTabId, activeTabType}`: one `type:"terminal"` tab per pane running the conversation (see Mapping for what "running" means). Empty when none is open. |
| `session.tabs.subscribe` `{worktree}` | Streams `{type:"snapshot", …tabs}` then `{type:"updated", …tabs}` when the tab list changes. `session.tabs.unsubscribe {worktree, subscriptionId?}` ends it. |
| `terminal.list` `{worktree?, handles?}` | `{terminals:[…], totalCount, truncated:false}`: one row per pane (`handle` = herdr's terminal id). |
| `terminal.read` `{terminal}` | `{terminal:{handle, status:"running", tail:[…lines], source:"screen", …}}`: the last 200 rows as text. |
| `terminal.subscribe` `{terminal, client:{id}, viewport?, capabilities?}` | The screen, then live changes. See Terminal streams. `terminal.unsubscribe {subscriptionId, client, requestId}` ends it. |
| `terminal.send` `{terminal, text?, enter?, interrupt?}` | `{send:{handle, accepted:true, bytesWritten}}`. A prompt (`text` with `enter:true`) takes the conversation's claim first; see Prompt turns. Approval keys are `text` with `enter:false` (see Approvals). |
| `terminal.clearBuffer` `{terminal}` | `{terminal:{handle, cleared:true}}` after Ctrl-L is sent into the pane. Not a prompt turn, so it claims nothing. |
| `agentSession.subscribeStatus` | Streams `{type:"snapshot", sessions:[…]}` then `{type:"status", session}` on change: one session per pane running a conversation. |
| `nativeChat.readSession` `{agent, sessionId, limit?, beforeOffset?}` | `{messages, hasMore, beforeOffset?}` (or `{error}`): the conversation's journal from its live window, paged by row cursor. See Chat view. |
| `nativeChat.subscribe` `{agent, sessionId, subscriptionId, limit?, capabilities?}` | Streams `snapshot`, then `appended` or `replacement`, with `error` and `end` as the app reads them. `nativeChat.unsubscribe {subscriptionId}` ends it. |
| `clipboard.startImageUpload`, `appendImageUploadChunk`, `commitImageUpload`, `abortImageUpload`, `saveImageAsTempFile` | Staged images. Commit and save return the path string the app puts in the prompt. See Images. |
| `preflight.detectAgents` | A bare array of installed agent ids. |
| `settings.get` | `{settings:{}}`: no default agent and none disabled. |
| `session.tabs.createTerminal` `{worktree, agent}` | `{tab:{type:"terminal", id, terminal, title}}`: a new herdr tab in the conversation's directory running the agent with no resume. See New sessions. |
| `notifications.subscribe` `{includeDesktopSuppressed?}` | Streams `{type:"ready", subscriptionId, epoch}`, then `{type:"notification", agentState, paneKey, notificationId, …}` frames: `agentState` is `done` for finished and `blocked` for needs input. `notifications.getMissedSince {lastSeenSeq, epoch?}` replays from memory. See Notifications. |
| `notifications.registerPush`, `unregisterPush` | `{registered:false}`: there is no FCM. |
| `notifications.testPush` | `{accepted:true}`, or `{accepted:false, reason:"unavailable"}` when no `notifications.subscribe` stream is open. A synthetic in-app notification (see Notifications). |
| `accounts.list` | `AccountsSnapshot`. See Plan usage. |
| `accounts.subscribe` | Streams `{type:"ready", subscriptionId, snapshot}`, then `{type:"snapshot", snapshot}` on change. `accounts.unsubscribe` ends it. |
| `files.readDir` `{worktree, relativePath?}` | A bare array `[{name, isDirectory, isSymlink}]`: one directory's entries, directories first. More than 5 000 entries is `directory_too_large`. |
| `files.read` `{worktree, relativePath}` | `{worktree, relativePath, content, truncated, byteLength}`: a text file, cut to 1 MiB (`truncated` says so; `byteLength` is the full size). A binary file is `binary_file`. |
| `files.readPreview` `{worktree, relativePath}` | `{worktree, relativePath, content (base64), isBinary:true, isImage:true, mimeType}` for png, jpg, jpeg, gif or webp up to 2 MiB. |
| `files.resolveTerminalPath` `{worktree, pathText, cwd?}` | `{worktree, relativePath, absolutePath, exists, isDirectory, openTarget?}`: a path a pane printed, resolved inside the project. A file has `openTarget: {kind:"worktree-file", relativePath, absolutePath}`. |
| `files.searchPaths` `{worktree, query?, limit?, excludePaths?}` | `{worktree, files:[{relativePath, basename, kind}], totalCount, truncated}`: project files whose path matches the query, `.gitignore` honoured. `limit` defaults to 16 and tops out at 200. |
| `git.status` `{worktree, includeLineStats?}` | `{entries:[{path, status, area, oldPath?, conflictStatus?, conflictStatusSource?, added?, removed?}], branch?, head?, upstreamStatus?}`. `area` is `staged`, `unstaged` or `untracked`. |
| `git.diff` `{worktree, filePath, staged}` | `{kind:"text", originalContent, modifiedContent}`, `{kind:"binary"}`, or `{kind:"too-large", byteLength}` when a side is over 1 MiB. |
| `git.history` `{worktree, limit?, baseRef?}` | `{items:[{id, parentIds, subject, displayId, author, timestamp}]}`, newest first. `timestamp` is milliseconds; `limit` defaults to 50 and tops out at 200. |
| `git.commitCompare` `{worktree, commitId}` | `{entries:[…]}`: the files one commit changed against its first parent. |
| `git.branchCompare` `{worktree, baseRef}` | `{summary:{baseRef, compareRef, changedFiles, status, baseOid, headOid, mergeBase, commitsAhead, errorMessage?}, entries:[…]}`. `status` is `ready`, `invalid-base`, `unborn-head`, `no-merge-base` or `error`. `compareRef` is the checked-out branch, `HEAD` when detached. |

Every other method, including the structured `agentSession.*` calls (`send`, `currentRequests`, `respondToApproval`, `respondToQuestion`, `create`, `createSupport`) and `worktree.create` and `agent.launch`, answers `error.code: "method_not_found"` with the request's `id`. Terminal-backed chat never calls them: its prompts ride `terminal.send`, its cards ride `interactivePrompt`, and new sessions ride `session.tabs.createTerminal` (the app falls through when `agentSession.create` is refused). Orca reads that as
"not available here" (`isMobileMethodUnavailableError`) and skips the read. Errors the bridge raises:
`bad_request`, `worktree_not_found`, `terminal_not_found`, `herdr_unavailable` (herdr's socket cannot be
reached or a pane's processes cannot be read), `unavailable` (the sesync state cannot be read, or a chat's transcript cannot be converted),
`send_failed`, `conversation_locked` (a turn holds the conversation; the message names the holder),
`invalid_relative_path`, `not_found`, `binary_file`, `file_too_large`, `not_a_file`, `not_a_directory` and
`directory_too_large` (file reads), `not_a_repository`, `git_refused` and `git_failed` (git reads).

Protocol version: the bridge speaks Orca's mobile protocol 3 (`MOBILE_PROTOCOL_VERSION`), and the
phone accepts a host with `minCompatibleMobileVersion` 2 or lower.

## Mapping

| Orca / app | sesync / herdr |
|---|---|
| host | this machine (the bridge) |
| worktree | one sesync conversation: its parent session and that session's mirrors. `worktreeId = <repoId>::<conversation id>`; `repoId = sesync-<hash of the project directory>`, the same id `repo.list` reports; `conversation id = conv-<hash of harness and parent key>`. Both hashes are stable while the parent session's file or store id is. |
| tab / terminal | one herdr pane running the conversation. A pane runs it when its foreground argv carries one of the conversation's session ids as a flag's value (`argv_carries`, the rule "open in herdr" uses), or the bridge opened the pane for it. When no pane does, the pane runs it when the conversation is the one its tab tag or title names: the Open-in-herdr tag ends the pane's tab label; else the pane's agent is the conversation's harness and its terminal title, with the harness's prefix removed (omp `π` then a spinner or `>`; Claude Code a glyph such as `✳`; OpenCode `OC \| `), is the conversation's title, and when several conversations share that title, the one in the pane's foreground directory. Never ambiguous: a conversation a pane already runs directly takes no fallback, and a fallback two panes both fit goes to neither. Handle and terminal id = herdr's `terminal_id`, which stays with the terminal when its pane moves. |
| agent status | herdr `agent_status`: `working` → `working`; `blocked` → `blocked`; `done` → `done`; `idle` or `unknown` → `waiting`. A tab row's `agentStatus` carries that `state`, `paneKey` (`tabId:terminalId`), `terminalHandle`, `agentType` (the conversation's harness), `providerSession.id` (the journal session id `nativeChat` reads by), and `interactivePrompt` when the pane is blocked on a recognised approval or question. Worktree status: working → `working`, blocked → `permission`, done → `done`, otherwise `active`/`waiting`. Session status (`agentSession.subscribeStatus`): working → `working`, blocked → `attention`, idle or done → `idle`, unknown → null. |

## Terminal streams

`terminal.subscribe` sends the screen as herdr renders it (`pane.read`, `source:"visible"`,
`format:"ansi"`, `strip_ansi:false`), with every line break as CR LF.

With `capabilities.terminalBinaryStream: 1` (what the app sends), the reply is a streamed JSON
`{type:"subscribed", streamId, lines:[], truncated:false, cols, rows, displayMode:"auto", seq:0}`, then
binary frames tagged with that `streamId`. Each frame is `header ‖ payload`, 16-byte header:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | kind `0x74` |
| 1 | 1 | version `0x01` |
| 2 | 1 | opcode |
| 3 | 1 | `0x00` |
| 4 | 4 | streamId (u32 LE) |
| 8 | 4 | seq high (u32 LE) |
| 12 | 4 | seq low (u32 LE) |

Opcodes used: `1` Output (UTF-8), `2` SnapshotStart (JSON meta `{kind:"scrollback", cols, rows, displayMode, truncated:false, truncatedByByteBudget:false, seq}`),
`3` SnapshotChunk (UTF-8), `4` SnapshotEnd (empty). Text is cut into pieces of at most 48 KiB on character
boundaries. Snapshot = SnapshotStart, its chunks, SnapshotEnd. A change is an Output carrying the full
repaint `ESC[H ESC[2J` followed by the new screen, so no row from the previous frame survives.

Without the capability the same screen goes as JSON: a `scrollback` result with `serialized`, then
`{type:"data", chunk}` events, each a full repaint. A phone without the capability is served either way.

The stream ends with `{type:"end"}` when the pane is gone or the stream is unsubscribed. Dropping the
connection stops its streams.

Orca's flow control (Ack) is for its multiplexer, which the phone does not use. Input from the phone
goes through `terminal.send`, not binary Input frames; resize and viewport claims are not implemented
(the pane keeps herdr's size).

### Why polling

herdr's `pane.updated` does not fire when a pane's output changes (checked against herdr 0.9.1: output
from `pane.send_keys` and `pane.send_text` produced no event). `pane.output_matched` fires only when a
matching line appears, and an events connection fixes its pane set when it opens, so it cannot follow
panes that come and go. The bridge therefore polls: a screen every 250 ms (sent only when it changed),
the tab and status lists every second, and the conversation list from disk every 5 seconds.
herdr's `pane.agent_status_changed` is not used for screens, but it is used for notifications: a per-pane `events.subscribe` delivers every status change (see Notifications), so no turn is missed between samples.

## Prompt turns

A `terminal.send` with text and `enter:true` is a prompt. Before the text is typed, the bridge claims the
conversation in sesync-lock (`claim <conversation> phone <bridge pid> <pane>`), the way a harness turn
does, so the TUI, the daemon and the other harnesses cannot write the same conversation at once.

- A claim another harness holds refuses the prompt with `conversation_locked`, naming the holder
  (`claude 4242`). Nothing is typed.
- A second prompt while the bridge's own turn runs is refused the same way.
- A pane with a detected agent gets the prompt through herdr's `agent.prompt` (text, then Enter, as one
  ordered submission). Any other pane gets the text, then `enter`.
- The claim is released when the agent is seen `working` or `blocked` and then `idle` or `done`. A
  prompt the agent never acts on releases after 15 s. Every turn releases by 30 min at the latest, so a
  stuck pane cannot lock a conversation. The release is `release <conversation> phone <bridge pid>`.

Other input (`text` without `enter`, `interrupt`, `clearBuffer`) is not a turn and is not claimed. `interrupt` sends
`ctrl+c`; `terminal.clearBuffer` sends `ctrl+l`.

## Framing: legacy only, and a correction

The brief said v2 framing (`e2ee_hello` with `v:2`, transcript hash) is needed only for relay. That is
not what the source shows. For direct connections, `rpc-client-socket-session.ts` dials **v2 first**
and falls back to legacy only if the host closes with `4001` before `e2ee_ready`, and only for a host
it has never seen speak v2 (`paired-e2ee-framing-policy.ts`).

So this bridge implements legacy framing and answers any v2 hello with close `4001` before ready. The
app then redials with the legacy hello. Cost: one refused dial on first pairing. Implementing v2 is
deferred; the interop check below covers this path.

## Security

- **Crypto is classical.** X25519 plus XSalsa20-Poly1305 is what Orca's legacy channel uses. It is
  acceptable here only because it runs inside WireGuard, which is itself classical. The post-quantum
  hybrid (ML-KEM) is agent-hub #6, and the homelab policy requires it for any new key exchange.
- **Pairing is the trust decision.** Whoever can read the offer can pair. Show the QR only to the
  phone being paired. Revoke with `sesync bridge revoke`.
- **A paired phone can type into your agents.** `terminal.send` writes text and Enter into the herdr
  panes that run your conversations, and `agent.prompt` submits prompts to their agents. Treat a paired
  phone as a keyboard on this machine's agent sessions. The conversation claim stops a phone from writing
  while the TUI or another harness is mid-turn, but it does not stop a phone from typing when they are idle.
- **Tokens:** 256 random bits; stored as SHA-256; compared in constant time; checked on every
  frame; a revoked device is refused on its next frame.
- **Network:** a peer outside the allowlist is dropped at accept, before any protocol bytes. Only
  IPv4 is bound. An IPv4-mapped IPv6 peer is checked against the same list.
- **Limits:** 1 MiB messages; each frame before auth must arrive within 10 s of the one before. A
  frame that does not decrypt is dropped without a reply. Terminal snapshots and frames are 48 KiB
  pieces; a screen is at most the pane's visible rows.
- **What a paired phone can read:** conversation titles, harness names, project directory paths, and
  the visible screen and last 200 rows of each pane that runs a conversation. Read-only, it can also
  read the files under each conversation's project directory and that repository's git status, diffs,
  history and branch comparisons (see Files and source control). Nothing else from the session stores is
  sent. Its only writes are `server.key`, `devices.json`, `pins.json`, the sesync-lock claims for prompt
  turns, and the upload staging.
- **Notifications and push.** The ntfy publish token is read once at startup and held in memory; it is never logged or written. Publishing is best effort on a detached thread: a failure is logged without the token and never blocks a notification.
- **Uploads.** Staged images live under `$SESYNC_CONFIG_DIR/bridge/uploads/<conversation>/` (directory `0700`, files `0600`), at most 20 MB decoded per image, and must start with a PNG, JPEG, GIF or WEBP signature. An upload that is refused or idle for 5 minutes is removed.
- **Approval keys.** A paired phone can answer an approval by typing its key into the pane (`terminal.send` with `enter:false`): the same reach as typing into the pane.
- **Not done yet:** per-device permissions (a paired phone has every method above), rate limiting,
  audit logging beyond connect/refuse lines, a revocation UI, key rotation for `server.key`, and a
  read-only mode.

## Files and source control (read-only)

The file explorer, file previews, terminal file links, the `@` file search and the Changes and Commits tabs read the conversation's project directory, the `dir` the conversation records. Every path the phone names is checked twice: lexically (no absolute path, no `..`, no NUL byte, no `.git` component), then again after symlinks are followed (a link that leaves the project, or points into `.git`, is `invalid_relative_path`). The check lives in `confine.rs`. `.git` is never listed or read, because its config can hold remote credentials.

Caps: a text file is read up to 1 MiB and marked `truncated` when cut; an image preview is at most 2 MiB; a directory lists at most 5 000 entries (more is `directory_too_large`, never a silent cut); `files.searchPaths` walks at most 50 000 entries and returns `limit` rows.

Git runs only through one runner (`git.rs`). Its allowlist is the verbs `status`, `diff`, `log`, `show`, `merge-base` and `rev-parse`, each with the read-only options the code lists: for example `--porcelain=v2 -z --branch` for status, `--no-ext-diff --no-textconv` for diff, and `--no-show-signature` for log and show, so a repository's own signature settings never run gpg. Any other verb, and any other option (`--output`, `--ext-diff`, `--textconv`, `-c`, `--git-dir`), is refused as `git_refused` before git starts. A revision from the phone is 1 to 255 characters from `[A-Za-z0-9._/@+-]` with no leading `-` and no `..`; a commit id is 40 or 64 hex digits; paths follow `--`. git runs with `-c core.fsmonitor= -c core.hooksPath=/dev/null` and `GIT_OPTIONAL_LOCKS=0`, with `GIT_DIR` and `GIT_WORK_TREE` removed from its environment, a 10 s limit and an 8 MiB output cap.

Apart from pins and `terminal.clearBuffer` (Ctrl-L into the pane), none of these methods writes.

## Chat view

The chat screen for a terminal-backed agent reads the conversation's journal, not the pane. `nativeChat.readSession` and `nativeChat.subscribe` take the journal session id from the tab's `agentStatus.providerSession.id` (for omp and Pi, the session file path, which `omp --resume` takes). omp and Pi journals are read in process: the window starts at the last compaction's kept entry, and the compaction shows as a system line. Tool calls and their results are linked by call id, thinking is shown as text, and an image is a label (its bytes never go on the wire). Claude and OpenCode are converted first, by the converters the daemon uses: a Claude transcript by `sesync-conv from-claude`, an OpenCode session by `opencode export` run in the session's project directory and then `sesync-conv from-opencode`. The converted copy is kept under `<bridge state dir>/transcripts/` (owner-only), one per source state, named by the conversation and a digest of the source's stamp (a file's size and change time, or the store's own counts for the session). A read converts only when the stamp moved, and older copies are removed. A subscription rebuilds the copy once the stamp has held still for a poll (one second), or at most every five seconds while it keeps moving; conversions run one at a time. A harness whose source cannot be stamped answers `unavailable`. Paging uses a row cursor: `beforeOffset` is a record's position in the file. A subscription checks the file's length and modification time each second; a grown window is `appended`, a moved one is `replacement`.

Composer prompts are `terminal.send` (see Prompt turns). The app sends them with bracketed paste already applied.

## Approvals

When herdr reports a pane `blocked`, the bridge reads its last 60 screen lines and recognises the harness's prompt. omp and OpenCode approval and question screens are captured in `sesync/tests/fixtures/approvals/`. The tab row then carries `agentStatus.interactivePrompt` as a JSON string: `{"approval":{"tool","summary"}}` for an approval, or `{"questions":[…],"toolName"}` for a question. The keys are typed into the pane: approve is Enter (`\r`) and deny is Esc, both sent by the app through `terminal.send` with `enter:false`. Claude Code is not recognised: fake-model cannot speak the Anthropic API, so its prompt format is unverified here.

## Images

`clipboard.startImageUpload {expectedBase64Length}` returns `{uploadId}`. `appendImageUploadChunk {uploadId, offset, contentBase64}` must arrive in order (`offset` equals the bytes so far), at most 512 Ki base64 characters per chunk. The total is limited to 24 Mi base64 characters and 20 MB decoded. `commitImageUpload` returns the staged absolute path as a string, which the app inserts into the prompt as `@path`. omp expands that into an image input, so the model request carries an image part (fake-model counts these as `images`). `saveImageAsTempFile {contentBase64}` does the same in one call. The app's clipboard params carry no `worktree`, so uploads stage under `_unscoped`.

## New sessions

`preflight.detectAgents` lists the agents on PATH from the table in `start.rs` (claude, omp, opencode, pi, and antigravity as `agy`). `session.tabs.createTerminal {worktree, agent}` opens an unfocused herdr tab in the conversation's directory and starts the agent with `agent.start`, with no resume argument. The new pane is remembered as belonging to that worktree (`terminals::remember_pane`), so `session.tabs.list` shows it before its agent has a session id. If herdr refuses `agent.start`, the tab the call made is closed and the call fails with `agent_not_started`.

## Notifications

A watcher subscribes to herdr's `pane.agent_status_changed` event for each pane it knows (`events.subscribe`, one subscription per pane), and applies every status change as it arrives, so a turn is seen however short it is. The pane list is re-read every second to follow panes that open or close, and the subscription is reopened then. Per pane, `working` → `idle` or `done` is finished, and any change into `blocked` that was not already blocked is needs input. The first observation of a pane records it and raises nothing. Events carry a sequence number and the process's epoch. The in-app stream sends `{type:"notification", source:"agent", title, body, worktreeId, paneKey (the terminal id), notificationId "<epoch>:<seq>", notificationSeq, notificationEpoch, agentState ("done" or "blocked"), emittedAt}`; the body is the conversation's title. The last 200 events stay in memory for `getMissedSince`. With push e…

`notifications.testPush` sends one synthetic frame, `{type:"notification", source:"test", title:"Test notification", notificationId:"test:<epoch>:<n>"}`, to every open `notifications.subscribe` stream. It is not kept in the history, takes no sequence number and never goes to ntfy. The reply is `{accepted:true}`, or `{accepted:false, reason:"unavailable"}` when no stream is open.

## Plan usage

`accounts.list` and `accounts.subscribe` answer the app's `AccountsSnapshot`: the `claude` and `codex` account lists and the `rateLimits.claude|codex.{session,weekly}` bars (`usedPercent`, `windowMinutes`, `resetsAt` in ms or null). The bridge maps `omp usage --json` to it: the 5-hour window is the session bar and the 7-day untiered window the weekly bar; Anthropic is `claude`, OpenAI Codex is `codex`. Every login is also an inactive row. omp does not say which login is the system default, so with exactly one login for a provider that login's usage also fills `rateLimits.claude` (or `codex`), the bars the app draws for the system default; with several logins those stay null and the app shows no default bars. Nothing else from the report is shown. A failed refresh keeps the last good snapshot, and a subscriber gets a frame only when the usage changes. The bridge never sends provider credentials to the phone. Verified live against the e2e fixture (one login: `rateLimits.claude.session.usedPercent` 25).

## Session data

`worktree.ps` and `repo.list` read the listing the TUI uses (`manage::list_groups`) on the saved
`state.json`, read-only, through each harness's own cwd or store row, and resolve each conversation's
herdr session identifiers through the harness's resume command. It does not run the daemon's save-time
steps, so a session the TUI hides only in memory can show here until the daemon next saves. Each request
re-reads the files it needs; the conversation list is not cached between requests (streams refresh it
every 5 seconds).

## Herdr access

The bridge talks to herdr over its Unix socket: one JSON request per line, one reply line per request,
each request on its own connection, 5 s timeout. The socket is `$HERDR_SOCKET_PATH`, else
`$HOME/.config/herdr/herdr.sock`; `--herdr-socket` overrides it. Methods used: `pane.list`, `pane.get`,
`pane.process_info`, `pane.read`, `pane.send_text`, `pane.send_keys`, `agent.prompt`, and `tab.list` (tab labels, read only when an agent pane is listed, for the tag rule). The bridge never
creates, closes, or resizes panes. Verified against herdr 0.9.1 (protocol 22):

- `pane.read` with `format:"ansi"` returns CRLF line endings and the colour codes herdr keeps.
- `pane.send_keys` takes names such as `enter`, `ctrl+c`, `esc`, `space`; `pane.send_text` types raw text.
- `pane.process_info` reports each foreground process's `argv`; a pane started with `sh … --resume <id>`
  reports that argv while the shell's loop runs.
- Real agent detection (`agent`, `agent_status`) needs a recognised agent binary; the tests use a shell
  stand-in, so `agent.prompt` and the status mapping for detected agents are covered by unit tests only.

## Verification

Run with a sandboxed HOME, as `AGENTS.md` requires.

```sh
cd sesync
SB=$(mktemp -d)
env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test --lib bridge
env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test --test bridge
env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test --test bridge_resources
```

The live tests in `tests/bridge.rs` (`a_prompt_typed…`, `an_approval_card…`, `an_uploaded_image…`, `a_new_agent_tab…`, `finished_and_needs_input…`) need herdr and omp on PATH and a fake-model build: `FAKE_MODEL_BIN`, or the static build in the sibling `sesync-wt-fake-model` worktree. Each skips with a note when they are missing.

- `src/bridge/*`: unit tests for the herdr reply and pane parsing, the conversation id and worktree
  mapping, the tab, terminal and status shapes, the binary frame codec and 48 KiB chunking, the
  stream registry, the prompt claims (against a stand-in sesync-lock script, including a holder being
  named and a timed release), the RPC dispatch against canned herdr answers, the pane fallback rules (the tag, the title and directory, ambiguity, and that an argv or owner match is never overridden), and the chat conversions (cached per source state, replaced when it moves, owner-only).
- `src/bridge/pins.rs`: pins survive a reload, the file is owner-only, a damaged file fails the write and is left alone, `isPinned` must be a boolean, and no other field is accepted. `rpc.rs` tests: the pin shows on the worktree row, `terminal.clearBuffer` sends Ctrl-L and reports a missing pane, write verbs and unserved methods answer `method_not_found`, and every read method is routed. `notify.rs`: a test push reaches a live stream, is kept out of the history, and is refused with no stream.
- `src/bridge/confine.rs`, `files.rs`, `git.rs`: the path confinement (`..`, absolute paths, NUL, `.git`, a symlink that leaves the project or enters `.git`), the size caps, the `.gitignore`-honouring search, the git allowlist (every other verb and each unlisted option is refused before git starts, and a write verb leaves the repository unchanged), and reads of real files and a real `git init` repository in a sandbox. `rpc.rs` also runs a live repository through dispatch.
- `src/bridge/e2ee.rs` pins the NaCl framing against a tweetnacl vector (both directions, and the
  server public key derivation).
- `tests/bridge.rs` drives a real loopback WebSocket server with a Rust client: hello, auth, status,
  `method_not_found`, bad token, revocation, v2 fallback, allowlist, the real `StateCatalog` over an
  omp fixture, and `worktree.ps` with and without herdr. Its last test starts a real `herdr server` in a
  sandboxed HOME (skipped with a message when herdr is not on PATH), runs a shell stand-in agent whose
  argv carries the session id, and then, over the encrypted link: lists the conversation, its tab and
  terminal, reads the screen, subscribes with the binary stream (the snapshot must show `hello`), types
  `ping` with Enter and reads the echo `got: ping` back as an Output frame, subscribes again with the
  JSON events, unsubscribes, and reads the status stream.
- `tests/bridge.rs` (Claude and OpenCode chats): a Claude transcript and an OpenCode store in a sandboxed HOME, with a fake `opencode` CLI that answers only from the session's directory and a fake herdr socket, are opened over the encrypted link. Tabs list, `nativeChat.readSession` reads both, a turn appended to the transcript and a row added to the store arrive on `nativeChat.subscribe`, and `terminal.send` reaches each pane as `agent.prompt`. `tests/opencode_export.rs` checks the export runs from the session's project directory.
- `tests/bridge_resources.rs` starts the built `sesync bridge serve` with no phone connected and asserts
  its resident memory is under 20 MB (measured 12–13 MB across runs).
- Phase 2 (`tests/bridge.rs`, live): a prompt typed into a live omp pane reaches `nativeChat.readSession`; an approval card denies without writing and approves by running the tool; an uploaded image reaches the model request log as an image part; a created tab is listed; finished and needs-input notifications reach the in-app stream and a local ntfy stand-in. Unit tests cover the transcript window and paging, the approval recognisers against the captured screens, upload staging and limits, the notification transitions and the ntfy request, the usage mapping, and the new RPC shapes.
- `fork/e2e/backend.sh up|down <stateDir>` (orca-phone) starts the same stack for the emulator suite and writes `<stateDir>/env`.

Interop with Orca's own TypeScript (`tools/bridge-interop`): it copies the Orca files it needs
(pairing codec and schema, compat gate, host status schema, method-unavailable rule, mock E2EE
framing) and runs 13 checks against a live bridge. It predates the phase-1 methods and does not cover
the terminal stream.

```sh
SB=$(mktemp -d)
env HOME="$SB" ./target/debug/sesync bridge pair --endpoint ws://127.0.0.1:6768 --label interop | grep '^url:'
env HOME="$SB" ./target/debug/sesync bridge serve --listen 127.0.0.1:6768 &   # stop it by PID afterwards
cd tools/bridge-interop && npm install && ORCA_PHONE_DIR=~/projects/orca-phone npm run interop -- "agenthub://pair?code=…"
```

Not verified here: a real phone, the emulator run (orca-phone `fork/e2e`), a live `omp usage --json` run, Claude Code approvals, and real ntfy delivery.
