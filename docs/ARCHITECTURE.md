# Architecture

Read [AGENTS.md](../AGENTS.md) first. This document explains the design; it is not a tutorial
for users (see [README.md](../README.md)). When this document and the code disagree, the code
and the tests are right; fix this document.

## The three layers

1. **Local sync.** A conversation has one canonical source on disk (an omp/Pi journal, a
   Claude transcript, or an OpenCode database row). The daemon detects changes, converts the
   canonical session into each *chosen* harness, and writes mirrors. Mirrors are derived data.
   A lock makes sure only one harness writes a conversation at a time.
2. **Team sync (client).** A user *marks* a session. After it has been quiet, the daemon builds
   the canonical omp/Pi journal for it, takes a server lease, seals the journal to the team's
   devices, signs the record, and pushes it. A pull lists the team's conversations, fetches
   records, and accepts only those that pass every check. Import (writing a pulled session into
   a local harness) is always a confirmed, separate action.
3. **Team server.** Stores opaque sealed records, issued tokens, leases and the admin-signed
   member list. It enforces write authorization and one-writer-at-a-time, but it never holds a
   key that can open a session and never parses plaintext.

```
harness stores ── scan ──> State ── mirror_write ──> other harness stores
                              │
                              │ marked + quiet + canonical
                              v
                       lease acquire ──> seal ──> POST /record/sealed ──> team-server
                                                                             │
                       TUI / daemon pull <── verify <── POST /record/sealed/read
```

## Local sync

- `discovery` finds candidate session files (omp, Pi, Claude transcripts, Gemini session
  logs) and OpenCode sessions. `scan` decides whether a source changed and is quiet enough
  (`SESYNC_QUIET`, default 20 s; `MIN_BYTES`; `SESYNC_MAX_AGE_DAYS`, default 14).
- A chosen mirror target with no mirror forces a pass. When a write keeps failing (an empty
  stub session, a conversion error), retries back off (`scan::mirror_demand`,
  `scan::mirror_retry_due`): the quiet period after the first failure, doubling to a 10-minute
  cap; a changed file and a successful pass reset it. The TUI shows a chosen target with no
  file as `pi?` (pending) or `pi!` (failing) in the MIRRORS column.
- Mirror **placement** follows the source session's own project directory, not the target
  store's layout: omp buckets are home-relative (`-SkillsOutlie` for `~/SkillsOutlie`) and are
  computed from the real home (`Env.home`, not the store's parent); Pi and Claude buckets are
  separator-replaced absolute paths. The cwd is read from the journal's `session` record head
  (`buckets::cwd_from_head`). A recorded mirror is stale (`scan::mirror_stale`) when its bucket
  disagrees with that cwd, the file is gone, or — for OpenCode — the session id is absent from
  `opencode.db`; the next pass rewrites it, and `mirror_write::record_mirror` replaces the
  record, deleting the superseded file only when it is unchanged.
- A source's identity is `(fmt, key)`. omp and Pi both use the journal format; their `key` is
  the session id. OpenCode's `key` is its session id (`ses_...`); its display title is
  recorded in `Source::title` at scan time because the source does not carry one on disk.
- `mirror_write` is the only writer. It claims the conversation lock (`with_claim`), then for
  omp, Pi, Claude and Gemini installs the converted file (`install::install_journal`,
  `install::install_gemini`) and records the created path in `State::mirror_keys` so the
  daemon never mistakes a mirror for a source. Claude and Gemini sources are converted to a
  journal first (`sesync-conv from-claude`, `from-gemini`): a raw transcript is not a journal
  and must never be copied through to another harness.
- The OpenCode target is an import, not a file: `install::install_opencode` converts the same
  journal to an import bundle (`sesync-conv to-opencode`) and runs `opencode import` with the
  conversation's directory as the working directory, so the session lands in the right project.
  `opencode import` conflict-ignores existing message ids, so the writer deletes the previous
  mirror id first (`opencode session delete`); the deterministic id
  (`mirror::mirror_opencode_id`) makes that a replace, never a duplicate. The mirror record
  carries the session id instead of a path, and it is not added to `mirror_keys` (that set
  names files, and a session id in it could hide a real OpenCode session from the scan).
- **Mirror ids are minted from the conversation's stable id, not from the moving source key**
  (`Source::conv`: the uuid/ses id of the store copy that started the conversation). A
  turn-taking promotion changes the source key; a key-derived id would mint a new copy in
  every other store and leave the old ones behind. With the stable id, each store's copy has
  one deterministic path for the conversation's lifetime and every refresh replaces it in
  place. States written before `conv` existed are re-minted once on the next pass.
- **Turn-taking** (`turn.rs`): a mirror is a copy, but the user may answer in the copy's
  harness. Every pass fingerprints each recorded mirror (one stat; a read only when the
  fingerprint moved, cached in `Mirror::edit_stamp`; OpenCode mirrors compare the store row's
  `updated` against `Mirror::target_updated`). When a copy's content no longer matches what
  the daemon installed, that copy is the newest statement of the conversation: the newest
  write wins (ties break by harness name), the copy becomes the session's source — the main
  harness moves — and the same pass rewrites every other configured copy from it without the
  quiet window, so no harness is left showing a stale conversation. The old source's harness
  becomes a mirror target and its stale record is carried over so the replaced file is
  deleted (`record_mirror`'s replace rule). One entry per conversation is enforced by
  component resolution (`turn::component_of`): entries that record each other as mirrors are
  one conversation; a component with a real edit turns to that edit, one without collapses to
  its most recently turned entry. Resurrection is closed at the merge points: the TUI prunes
  sources the daemon removed (`absorb_daemon_state`, keeping runtime-only `tui_created`
  choices) and the daemon does not re-adopt keys hidden in `mirror_keys`.
- The Gemini target is a file inside a scanned store: `sesync-conv to-gemini` writes one
  `session-<stamp>-<short>.jsonl` under `$GEMINI_CLI_HOME/tmp/<slug>/chats`, resolving the
  project's slug exactly as Gemini does (an existing `projects.json` entry, else the
  `.project_root` marker, else the slugified directory name with `-1`, `-2` suffixes), and
  records the project in `projects.json` with a `.project_root` marker so
  `gemini --list-sessions` finds the session. The file name is deterministic per (source,
  mirror id), so a refresh atomically replaces the same file; because `discovery` scans the
  same tree, the mirror path **is** added to `mirror_keys` (unlike OpenCode's session id).
- **`Source::chosen_targets` is the complete write policy.** Empty (the default) means the
  daemon writes nowhere. The TUI picker sets it per session; it never offers the session's own
  format, and it only offers harnesses installed on the machine.
- Mirrors are rebuilt when the canonical source changes; a locked conversation is skipped, not
  queued. `mirror_cleanup` removes copies the daemon previously made when a target is dropped.

## Team lifecycle

- **Create** (`sesync team create`): generates the admin Ed25519 key (kept on the operator's
  machine at `<config>/admin/device.key`, mode 0600), creates the space, signs member list v1,
  and prints a one-time TOTP URI. The admin private key never goes to the server.
- **Join** (`sesync team join`): the member proves the TOTP, sends its device's public keys,
  and stores a credential under `~/.config/sesync` (or `SESYNC_CONFIG_DIR`). The credential
  contains the token, the machine id, the pinned admin public key and the signed list it
  received. That pinned key is the trust root: it is the only thing that can widen who this
  device accepts.
- **Publish** (`sesync team publish`): fetches the enrolled devices from `/admin/devices`,
  builds a list at version `current + 1` naming the admin and those devices, signs it with the
  admin key, and posts it to `/admin/list`. The server refuses to move the list backwards.
- **Forget** (`sesync team forget <member> <machine>`): revokes the device's token at once;
  the next publish signs it out of the member list. Revocation narrows the recipient set on
  the next seal.
- **Renew** (`sesync team renew`): renews a token while the enrolment is inside its window;
  after that the TOTP is needed again.

Membership levels: the server's admin token authorizes `/admin/*`; a device token
(machine-scoped) authorizes member routes. A token alone is not enough for a write: the device
must also still be enrolled, and the record must be signed by a device the current list names.

## Push path (exactly what runs)

1. `State::marked` holds the sessions the user marked. Nothing else is eligible.
2. `team_sync::is_due` requires the source to be quiet for `SESYNC_TEAM_QUIET` (default
   120 s) and the plaintext hash to differ from the last successful push.
3. `team_sync::canonical_for_source` produces the team copy: the omp/Pi journal as-is for
   omp/Pi sources, `sesync-conv from-claude` for Claude, `from-gemini` for Gemini, and
   `opencode export` + `sesync-conv from-opencode` for OpenCode. Reasoning is kept only where
   the converter can carry it as ordinary text; signed Claude thinking is dropped.
4. `team_push` acquires the lease. **The lease version is the record sequence.**
5. `sealed::seal_for_devices` compresses, encrypts, wraps the key to every listed device,
   and signs a v2 record bound to `(space, conversation, sequence)`.
6. `HttpTeam::put` posts to `/record/sealed?space&conversation&machine&version`. The server
   checks the token, the exact lease version, the signature, the author's place in the
   current list and the record's sequence equal to that version, then stores the bytes.
7. On success the daemon records `{hash, pushed_at}` in `State::team`. Sealing is randomized,
   so the change check must never hash sealed bytes (there is a regression test for this).

## Pull path (exactly what runs)

1. `POST /sessions` returns conversation ids and, when the stored bytes carry one, each
   conversation's current record sequence. That number is advisory only: it can save a fetch,
   never authorize an accept.
2. The client skips a fetch only when it has a persisted floor and the advertised sequence is
   not above it. No floor, or no advertised number, means fetch.
3. `POST /record/sealed/read` returns the stored record bytes.
4. `open_record` checks, in order: v2 magic, parse, expected space, expected conversation,
   list version not below the reader's verified list, author listed in that list, signature,
   and sequence at or above the reader's floor. A failure is counted and dropped; it is never
   shown.
5. `team_pull_verified` merges accepted sequences upward into `State::team_seen`. The floor
   only ever rises. A record below it is a rollback and stays refused.
6. The daemon only displays what passed. Writing a pulled session into a harness is an
   explicit user action (`team_import`, an `i` key press from the TUI).

## Trust model

What the server can do: refuse writes, withhold records, return an old record, or lie about
the sequence list. What it cannot do: read or forge a session, add a device to the list, make
a client accept an author that the pinned list does not name, or roll a client below its
floor. The pinned admin key is the root; the server merely stores the admin's signature.
Every use re-verifies the list: `Trust::verifier()` parses and checks the signature before
any seal or open, so a fetched list can only narrow or widen within what the admin signed.

The client trusts the *newest* verified list it has seen (`fetch_and_verify` refuses a lower
version). A writer whose list is behind the server's will have its record refused: it must
refresh before sealing. This is deliberate, so a device cannot seal under a list that already
revoked someone.

## Wire formats

### Record v2 (`sesync-team/src/signed_records.rs`)

```
"SR2" | author(32) | list_version(8) | sequence(8)
      | space_len(2) | space | conv_len(2) | conversation
      | sig(64) | content
```

All integers big-endian. `sig` is Ed25519 over every byte except the 64 signature bytes.
`sequence` is the lease version. A record that does not start with `SR2` is refused as
`UnsupportedFormat`; v1 is not converted. There is no production v1 data.

### Record content (`sesync/src/sealed.rs`)

```
u32 wrap_count | { u32 device_len, device(32), u32 sealed_len, sealed } * wrap_count | body
body = codec(1) | data
codec 0x00 = stored, 0x01 = zstd
```

Wraps are `crypto_box` sealed boxes: the per-record random data key, sealed to each listed
device's X25519 box key. The body is ChaCha20-Poly1305 with associated data

```
u64 space_len | space | u64 conv_len | conversation | u64 sequence
```

so a body cannot be moved to another space, conversation or sequence even inside a valid
signature. Max session size: 64 MiB before compression and after decompression; decompression
is chunked with a bounded window, so a crafted record cannot exhaust memory. zstd level 3;
if compression does not shrink the session, the codec byte says stored.

### Member list (`sesync-team/src/membership.rs`)

The admin signs a versioned list naming each device's Ed25519 signing key, X25519 box key and
member/machine identity. The server refuses a list version that does not increase. Clients
verify the signature against the pinned admin key on every use. See the module for the exact
encoding.

## Persistence

Client state (`<SESYNC_DIR>/state.json`, default `~/.local/share/sesync/state.json`):

| Key | Meaning |
|---|---|
| `sources` | per `(fmt,key)`: `conv`, `last_seen_change`, `last_hash`, `last_stamp`, `failures`, `last_failure`, `mirrors`, `chosen_targets`, `title`, `turn_at` |
| `mirror_keys` | every path/id the daemon created; never treated as a source |
| `subagents` | parent session → subagent transcript paths |
| `marked` | sessions eligible for team push |
| `team` | per session: `{hash, pushed_at}` of the last successful push |
| `team_seen` | per conversation: highest accepted team sequence (rollback floor) |

A missing state file is empty state; a corrupt one is an error (silently dropping it would
forget every mirror and risk duplicate writes).

Credentials and keys. The CLI, the TUI and the daemon all use the same config dir:
`SESYNC_CONFIG_DIR`, default `~/.config/sesync`. A credential written by `team join` is
therefore visible to the daemon without any copying.

| Path | Contents |
|---|---|
| `~/.config/sesync/teams/<space>.cred` | token, member, machine, pinned admin key, signed list, list version; mode 0600 |
| `~/.config/sesync/device.key` | this machine's device Ed25519 + X25519 keys; mode 0600 |
| `~/.config/sesync/admin/device.key` | admin key, only on the operator's machine |

Server store (`sesync-team/src/pg.rs`), one schema per deployment:

| Table | Purpose |
|---|---|
| `spaces` | wrapped space key |
| `records` | `(space, conversation) → signed record bytes` (opaque) |
| `trust` | current admin-signed member list |
| `admin_keys` | the admin public key pinned at create |
| `devices` | enrolled device entries by machine |
| `enrolled` | enrolment timestamps |
| `gates` | join approvals |
| `owners` | space owners |
| `backups` | per-space backup policy |

## HTTP routes (`sesync-team/src/server.rs`)

| Route | Auth | Purpose |
|---|---|---|
| `GET /healthz` | none | liveness |
| `GET /metrics` | none | counters |
| `POST /admin/team` | admin token | create a space; returns the TOTP URI |
| `POST /admin/invite` | admin token | invite a member |
| `POST /enrol` | TOTP | join; issues the device token |
| `POST /renew` | token | renew a token inside its window |
| `POST /lease/acquire`, `/lease/release` | member token | one writer per conversation |
| `POST /sessions` | member token | conversation ids + advisory record sequences |
| `POST /list` | member token | current admin-signed list |
| `POST /admin/list` | admin token | publish a signed list |
| `POST /admin/devices` | admin token | enrolled devices, for publishing |
| `POST /admin/forget` | admin token | revoke a device immediately |
| `POST /record/sealed`, `/record/sealed/read` | member token | the live sealed push/pull |
| `POST /record`, `/record/read` | member token | legacy plaintext routes, still used by old tests only |

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `SESYNC_DIR` | `~/.local/share/sesync` | sync state and archives |
| `SESYNC_LOCK_DIR` | `~/.local/share/harness-sync/locks` | lock files |
| `OMP_AGENT_DIR` | `~/.omp/agent` | omp source |
| `PI_AGENT_DIR` | `~/.pi/agent` | Pi source |
| `CLAUDE_CONFIG_DIR` | `~/.claude` | Claude source |
| `GEMINI_CLI_HOME` | `~/.gemini` | Gemini store: session sources and mirror target |
| `OPENCODE_DB` | `~/.local/share/opencode/opencode.db` | OpenCode source |
| `OPENCODE_BIN` | `opencode` | OpenCode CLI used for export (source) and import (mirror target) |
| `SESYNC_CONV` | beside the binary, else `~/.local/bin/sesync-conv` | converter |
| `SESYNC_LOCK` | beside the binary, else `~/.local/bin/sesync-lock` | lock binary |
| `SESYNC_QUIET` | `20` | local quiet seconds before a mirror is rebuilt |
| `SESYNC_MAX_AGE_DAYS` | `14` | ignore sources older than this |
| `SESYNC_POLL` | `5` | daemon tick and base team interval |
| `SESYNC_PASSES` | unlimited | run N passes then exit (testing) |
| `SESYNC_MACHINE` | hostname | machine id for joining |
| `SESYNC_CONFIG_DIR` | `~/.config/sesync` | team credentials, device key and admin key (CLI, TUI, daemon) |
| `SESYNC_TEAM_URL` | unset | team server base URL; team side is off without it |
| `SESYNC_TEAM_TOKEN`, `SESYNC_TEAM_MACHINE`, `SESYNC_TEAM_SPACE` | from credential | optional overrides |
| `SESYNC_TEAM_QUIET` | `120` | team quiet seconds before a push is due |
| `SESYNC_TEAM_POLL_MAX` | `60` | cap for the team poll backoff |
| `SESYNC_ADMIN_TOKEN_FILE` | unset | admin token for `sesync team` commands |
| `HS_TEAM_URL`, `HS_TEAM_TOKEN`, `HS_TEAM_MACHINE` | unset | TUI remote view (legacy names) |
| `HS_E2E`, `HS_REQUIRE_CLAUDE` | unset | opt-in real-CLI tests |

Unknown or malformed values fall back to the default; a bad env var must never panic the
daemon.

## Failure behaviour

| Situation | What happens |
|---|---|
| Another machine holds the lease | push fails, the session stays marked; retried after the quiet rule |
| Record sealed under an older list than the server's | server refuses (403); the client must `fetch_and_verify` and re-seal |
| Sequence below the reader's floor | dropped and counted as rolled back; never shown |
| Author not in the current list | dropped and counted; never shown |
| Record arrives for another space/conversation | refused before decryption |
| Session > 64 MiB | refused on seal and on open |
| Conversation locked locally | local mirror write skipped this pass |
| Team server unreachable | team step reports the error; local sync continues |
| Corrupt state file | daemon reports an error; it does not silently reset state |
| Mirror target removed | no new writes there; `mirror_cleanup` can remove what was written |

## Live vs test-only modules

- Live: `team_pull_verified`, `team_watch`, `team_pass`, `team_push`, `team_sync`,
  `team_backoff`, `sealed`, `team_join`, `team_cli`, `device_keys`, `mirror_write`,
  `remote`/`remote_http` (TUI view), `team_import`.
- Test-only today: `team_pull` (superseded by the verified pull), `team_conflict` (display
  helper with tests but no runtime caller), `divergence` (guarded the old mirror fan-out;
  only its tests reference it). Do not build on these without deciding to revive them.

## Rules for changes

1. The server never gets a key or a plaintext session. If a change needs either, the change
   is wrong.
2. A record format change needs a new magic (`SR3`); never reinterpret `SR2`.
3. Sequences, list versions and trust never move backwards. If a change could lower any of
   them, stop.
4. The canonical team copy stays the omp/Pi journal, whatever the source harness.
5. Every invariant in [AGENTS.md](../AGENTS.md) keeps its test; new edges get new tests.
6. Update this document with any wire, route, env or storage change, and update
   [STATUS.md](STATUS.md) at the end of the session.
