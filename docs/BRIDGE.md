# Phone bridge (`sesync bridge`)

A WebSocket server that lets the Orca Android app pair with this machine and list its sessions.
It is a spike (agent-hub #4): enough of Orca's mobile protocol to pair and to render a home screen.
It is not a remote control. Only the `sesync bridge` subcommand runs it; the daemon, the TUI and the
team client never load it.

Orca is MIT-licensed (stablyai/orca). Its protocol is read from the source at the commit in
`~/projects/orca-phone` (`mobile/src/transport/*`, `mobile/scripts/mock-server*.ts`,
`src/shared/pairing.ts`, `src/shared/mobile-relay-pairing-offer.ts`).

## Commands

| Command | What it does |
|---|---|
| `sesync bridge pair [--endpoint ws://HOST:PORT] [--label NAME]` | Pairs a new device. Prints the pairing offer (v2 JSON), the `orca://pair?code=` URL and a terminal QR. The device token is in that output and nowhere else. |
| `sesync bridge serve [--listen ADDR:PORT] [--allow CIDR,…]` | Serves in the foreground until Ctrl-C. Defaults: `0.0.0.0:6768`, allowed networks `10.10.20.0/24,192.168.1.0/24,127.0.0.0/8`. |
| `sesync bridge devices` | Lists paired devices: id, label, creation time. Never prints tokens. |
| `sesync bridge revoke <device-id>` | Removes a device. Its token is refused on the next frame, with no restart. |

`--endpoint` defaults to `ws://10.10.20.3:6768`, this machine's WireGuard mesh address. The phone
must reach that address (WireGuard) or the LAN address the offer names. Change `--endpoint` for
another route. Nothing here opens a firewall port: the host's firewalld zone decides what is reachable.

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
- Orca's `PairingOfferSchema` (run through `tsx`) accepts the offer for the default endpoint and
  for a loopback endpoint. See the interop check below.

## Wire protocol (subset)

Frames are WebSocket text messages. A plaintext frame is JSON. An encrypted frame is
`base64(nonce ‖ box)`, where `box` is NaCl's `crypto_box` output under
`shared = nacl.box.before(serverPublic, clientSecret)` with a random 24-byte nonce.
Both sides use the same layout as Orca's `mobile/src/transport/e2ee.ts` and its mock server.

1. Client → `{"type":"e2ee_hello","publicKeyB64":…}` (plaintext). Bridge → `{"type":"e2ee_ready"}`.
2. Client → encrypted `{"type":"e2ee_auth","deviceToken":…}`. Bridge → encrypted
   `{"type":"e2ee_authenticated"}`, or encrypted `{"type":"e2ee_error","error":{"code":"unauthorized"}}`
   and close `4001`.
3. Client → encrypted `{"id","deviceToken","method","params"}`. Bridge → encrypted
   `{"id","ok":true,"result",…}` or `{"id","ok":false,"error":{"code","message"}}`. Each request
   carries the token; it is checked on every frame.

Served methods:

| Method | Result |
|---|---|
| `status.get` | `{runtimeId, appVersion, protocolVersion:3, minCompatibleMobileVersion:2, capabilities:[], floatingWorkspaceEnabled:false, hostPlatform, machineName}` |
| `worktree.ps` | `{worktrees:[…], totalCount, truncated:false}`: one row per session project directory, newest first. Rows have `workspaceKind:"folder-workspace"`, no agents, no terminals, no PR links. |
| `repo.list` | `{repos:[…]}`: the same directories as repos. |

Every other method answers `error.code: "method_not_found"` with the request's `id`. Orca reads that as
"not available here" (`isMobileMethodUnavailableError`) and skips the read. Its home and host screens
degrade rather than fail.

Protocol version: the bridge speaks Orca's mobile protocol 3 (`MOBILE_PROTOCOL_VERSION`), and the
phone accepts a host with `minCompatibleMobileVersion` 2 or lower.

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
- **Tokens:** 256 random bits; stored as SHA-256; compared in constant time; checked on every
  frame; a revoked device is refused on its next frame.
- **Network:** a peer outside the allowlist is dropped at accept, before any protocol bytes. Only
  IPv4 is bound. An IPv4-mapped IPv6 peer is checked against the same list.
- **Limits:** 1 MiB messages; each frame before auth must arrive within 10 s of the one before. A
  frame that does not decrypt is dropped without a reply.
- **What a paired phone can read:** session titles, harness names, and project directory paths
  (`worktree.ps`, `repo.list`). Nothing else. Session content is never sent. The bridge never
  writes to the session stores; its only writes are its own `server.key` and `devices.json`.
- **Not done in the spike:** per-device permissions, rate limiting, audit logging beyond
  connect/refuse lines, a revocation UI, and key rotation for `server.key`.

## Session data

`worktree.ps` reads the same listing the TUI uses (`manage::list_groups`) on the saved
`state.json`, read-only, through each harness's own cwd or store row. It does not run the daemon's
save-time steps, so a session the TUI hides only in memory can show here until the daemon next saves.
Each request re-reads the files it needs (one head read per file-harness session); caching is not done.

## Verification

Run with a sandboxed HOME, as `AGENTS.md` requires.

```sh
cd sesync
SB=$(mktemp -d)
env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test --lib bridge
env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test --test bridge
```

- `src/bridge/e2ee.rs` pins the NaCl framing against a tweetnacl vector (both directions, and the
  server public key derivation).
- `tests/bridge.rs` drives a real loopback WebSocket server with a Rust client: hello, auth, status,
  `method_not_found`, bad token, revocation, v2 fallback, allowlist, and the real `StateCatalog`
  over an omp fixture.

Interop with Orca's own TypeScript (`tools/bridge-interop`): it copies the Orca files it needs
(pairing codec and schema, compat gate, host status schema, method-unavailable rule, mock E2EE
framing) and runs 13 checks against a live bridge.

```sh
SB=$(mktemp -d)
env HOME="$SB" ./target/debug/sesync bridge pair --endpoint ws://127.0.0.1:6768 --label interop | grep '^url:'
env HOME="$SB" ./target/debug/sesync bridge serve --listen 127.0.0.1:6768 &   # stop it by PID afterwards
cd tools/bridge-interop && npm install && ORCA_PHONE_DIR=~/projects/orca-phone npm run interop -- "orca://pair?code=…"
```

Not verified: a real phone, the QR code read by a scanner, and the Android app's own UI. The QR is
drawn with the same half-block renderer as the TUI's team QR; no decoder was available here.
