# Phone bridge (`sesync bridge`)

A WebSocket server that lets the Orca-derived Android app (agent-hub) pair with this machine and work
with its sesync conversations: list them, open one, watch its herdr pane live, and type a prompt into it.
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
| `sesync bridge pair [--endpoint ws://HOST:PORT] [--label NAME]` | Pairs a new device. Prints the pairing offer (v2 JSON), the `orca://pair?code=` URL and a terminal QR. The device token is in that output and nowhere else. |
| `sesync bridge serve [--listen ADDR:PORT] [--allow CIDR,…] [--herdr-socket PATH]` | Serves in the foreground until Ctrl-C. Defaults: `0.0.0.0:6768`, allowed networks `10.10.20.0/24,192.168.1.0/24,127.0.0.0/8`, herdr socket `$HERDR_SOCKET_PATH` else `$HOME/.config/herdr/herdr.sock`. |
| `sesync bridge devices` | Lists paired devices: id, label, creation time. Never prints tokens. |
| `sesync bridge revoke <device-id>` | Removes a device. Its token is refused on the next frame, with no restart. |

`--endpoint` defaults to `ws://10.10.20.3:6768`, this machine's WireGuard mesh address. The phone
must reach that address (WireGuard) or the LAN address the offer names. Change `--endpoint` for
another route. Nothing here opens a firewall port: the host's firewalld zone decides what is reachable.

`serve` takes the sesync-lock binary and its claim directory from the same environment the daemon uses
(`SESYNC_LOCK`, else the binary beside `sesync`; `SESYNC_LOCK_DIR`, else the stored locks directory). If
the lock binary is not installed, prompts are sent without a claim, as the daemon's own writes are.

Exit codes: 0 success, 2 usage, 1 failure (as in `sesync team`).

## Files

Under `$SESYNC_CONFIG_DIR/bridge/` (default `~/.config/sesync/bridge/`). The directory is `0700`.

- `server.key`: `0600`. The bridge's X25519 secret, base64. Created on first use, never replaced
  silently (a new key would strand every paired phone). The public key is derived from it.
- `devices.json`: `0600`. Per device: `id`, `label`, `created` (unix seconds), `token_sha256`.
  The token itself is never stored: it is 256 random bits, and only its SHA-256 is kept.

Writes are atomic (temp file, then rename) and created owner-only from the start.

## Pairing offer v2

The offer is the JSON below, encoded as unpadded base64url and carried as `orca://pair?code=…`
(the same encoding Orca's `encodePairingOffer` produces). The QR code carries the same URL.

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
| `worktree.ps` | `{worktrees:[…], totalCount, truncated:false}`: one row per sesync conversation, newest first. `status` and `agents` come from the herdr panes running it (see Mapping). Without herdr the rows still load, as not open. |
| `repo.list` | `{repos:[…]}`: the conversations' project directories, for grouping. Ids match the rows' `repoId`. |
| `session.tabs.list` `{worktree:"id:<wt>"}` | `{worktree, publicationEpoch, snapshotVersion, tabs:[…], activeTabId, activeTabType}`: one `type:"terminal"` tab per pane running the conversation. Empty when none is open. |
| `session.tabs.subscribe` `{worktree}` | Streams `{type:"snapshot", …tabs}` then `{type:"updated", …tabs}` when the tab list changes. `session.tabs.unsubscribe {worktree, subscriptionId?}` ends it. |
| `terminal.list` `{worktree?, handles?}` | `{terminals:[…], totalCount, truncated:false}`: one row per pane (`handle` = herdr's terminal id). |
| `terminal.read` `{terminal}` | `{terminal:{handle, status:"running", tail:[…lines], source:"screen", …}}`: the last 200 rows as text. |
| `terminal.subscribe` `{terminal, client:{id}, viewport?, capabilities?}` | The screen, then live changes. See Terminal streams. `terminal.unsubscribe {subscriptionId, client, requestId}` ends it. |
| `terminal.send` `{terminal, text?, enter?, interrupt?}` | `{send:{handle, accepted:true, bytesWritten}}`. A prompt (`text` with `enter:true`) takes the conversation's claim first; see Prompt turns. |
| `agentSession.subscribeStatus` | Streams `{type:"snapshot", sessions:[…]}` then `{type:"status", session}` on change: one session per pane running a conversation. |

Every other method answers `error.code: "method_not_found"` with the request's `id`. Orca reads that as
"not available here" (`isMobileMethodUnavailableError`) and skips the read. Errors the bridge raises:
`bad_request`, `worktree_not_found`, `terminal_not_found`, `herdr_unavailable` (herdr's socket cannot be
reached or a pane's processes cannot be read), `unavailable` (the sesync state cannot be read),
`send_failed`, `conversation_locked` (a turn holds the conversation; the message names the holder).

Protocol version: the bridge speaks Orca's mobile protocol 3 (`MOBILE_PROTOCOL_VERSION`), and the
phone accepts a host with `minCompatibleMobileVersion` 2 or lower.

## Mapping

| Orca / app | sesync / herdr |
|---|---|
| host | this machine (the bridge) |
| worktree | one sesync conversation: its parent session and that session's mirrors. `worktreeId = <repoId>::<conversation id>`; `repoId = sesync-<hash of the project directory>`, the same id `repo.list` reports; `conversation id = conv-<hash of harness and parent key>`. Both hashes are stable while the parent session's file or store id is. |
| tab / terminal | one herdr pane whose foreground argv carries one of the conversation's session ids as a flag's value (`argv_carries`, the rule "open in herdr" uses). Handle and terminal id = herdr's `terminal_id`, which stays with the terminal when its pane moves. |
| agent status | herdr `agent_status`: `working` → worktree status `working` / agent state `working`; `blocked` → `permission` / `blocked`; `done` → `done`; `idle` or `unknown` → `active` / `waiting`. Session status: working → `working`, blocked → `attention`, idle or done → `idle`, unknown → null. |

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
herdr's `pane.agent_status_changed` is not used for the same reason.

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

Other input (`text` without `enter`, `interrupt`) is not a turn and is not claimed. `interrupt` sends
`ctrl+c`.

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
  the visible screen and last 200 rows of each pane that runs a conversation. Nothing else from the
  session stores is sent; the bridge never writes to them. Its only writes are `server.key`,
  `devices.json`, and the sesync-lock claims for prompt turns.
- **Not done yet:** per-device permissions (a paired phone has every method above), rate limiting,
  audit logging beyond connect/refuse lines, a revocation UI, key rotation for `server.key`, and a
  read-only mode.

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
`pane.process_info`, `pane.read`, `pane.send_text`, `pane.send_keys`, `agent.prompt`. The bridge never
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

- `src/bridge/*`: unit tests for the herdr reply and pane parsing, the conversation id and worktree
  mapping, the tab, terminal and status shapes, the binary frame codec and 48 KiB chunking, the
  stream registry, the prompt claims (against a stand-in sesync-lock script, including a holder being
  named and a timed release), and the RPC dispatch against canned herdr answers.
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
- `tests/bridge_resources.rs` starts the built `sesync bridge serve` with no phone connected and asserts
  its resident memory is under 20 MB (measured 12–13 MB across runs).

Interop with Orca's own TypeScript (`tools/bridge-interop`): it copies the Orca files it needs
(pairing codec and schema, compat gate, host status schema, method-unavailable rule, mock E2EE
framing) and runs 13 checks against a live bridge. It predates the phase-1 methods and does not cover
the terminal stream.

```sh
SB=$(mktemp -d)
env HOME="$SB" ./target/debug/sesync bridge pair --endpoint ws://127.0.0.1:6768 --label interop | grep '^url:'
env HOME="$SB" ./target/debug/sesync bridge serve --listen 127.0.0.1:6768 &   # stop it by PID afterwards
cd tools/bridge-interop && npm install && ORCA_PHONE_DIR=~/projects/orca-phone npm run interop -- "orca://pair?code=…"
```

Not verified here: a real phone and the Android app's own screens (the emulator run is agent-hub's E2E,
against a sandboxed herdr and a fixture conversation), the QR code read by a scanner, and real agent
detection by herdr.
