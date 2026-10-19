# AGENTS.md

How to work in this repo without breaking it. Read this first, then
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/STATUS.md`](docs/STATUS.md) for the last known state. STATUS is a snapshot, not truth:
when it disagrees with the code or the tests, the code and the tests win, and you fix STATUS.

## What this is

DoppelClanker keeps one conversation in sync across harnesses (omp, Pi, Claude Code,
OpenCode) on one machine, and across machines for a team. The team server stores sealed
records only; it can check who wrote what and refuse a write, but it cannot read a session.
One harness writes a conversation at a time, enforced by a lock.

## Hard safety rules

1. **Never run a real harness CLI** (omp, Pi, OpenCode, Claude Code, Gemini) as part of
   development or testing, except in a throwaway container with `HS_E2E=1`. Tests that do
   run CLIs are opt-in for that reason.
2. **Never read or write the operator's real session stores**: `~/.omp`, `~/.pi`,
   `~/.claude`, `~/.gemini`, `~/.local/share/opencode`, or anything under `~/.local/share/sesync`.
   Tests must use paths under a temporary directory.
3. **Sandbox `HOME` for every test run**, and pass the real toolchain paths through, because
   cargo and rustup live in the real home:

   ```sh
   SB=$(mktemp -d /tmp/sesync-test.XXXX)
   env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test
   ```

   The right-hand `$HOME` is expanded by the calling shell before `env` overrides it.
4. **Kill processes by PID only.** Never `pkill -f sesync`, `pkill cargo`, or similar.
5. **Never commit, amend, push, or open a PR unless the operator explicitly asks.** Work is
   delivered as a green working tree plus a report.
6. **Never edit `~/hlgit` (homelab GitOps), Kubernetes, or the team server deployment**
   without loading the `homelab-context` and `deploy-to-homelab` skills first.
7. **Never log or commit secrets**: tokens, device keys, admin keys, credentials, master
   keys. Session content is as sensitive as the token: never paste it into a log or report.
8. **Never weaken or delete an assertion to get a green suite.** Fix the code, or report the
   failure with the exact error.

## Build and test

There is no root Cargo workspace. Each crate is its own workspace.

```sh
sh build-test.sh                 # every crate, the safe default
```

Per crate (all tests sandbox their own paths; a sandboxed HOME is still recommended):

```sh
cd sesync-lock  && env HOME="$(mktemp -d)" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test
cd sesync-conv  && env HOME="$(mktemp -d)" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test --release
cd sesync       && env HOME="$(mktemp -d)" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test
cd sesync-team  && env HOME="$(mktemp -d)" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" cargo test
```

### Live team tests

Some sesync tests start the real `team-server` process and a real Postgres. They are the
acceptance gate for any team wire change.

```sh
# 1. The tests launch this binary; rebuild it after every sesync-team change.
cd sesync-team
env HOME=/tmp CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" \
    CARGO_TARGET_DIR=/tmp/opencode/target-e2e cargo build --bin team-server

# 2. Postgres for the tests (throwaway container, not the homelab database).
podman start tsv-ses-pg   # 127.0.0.1:55437, user postgres, password test

# 3. Run the acceptance suite.
cd ../sesync
SB=$(mktemp -d)
env HOME="$SB" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" \
    TEAM_TEST_PG="postgres://postgres:test@127.0.0.1:55437/postgres" \
    TEAM_SERVER_BIN=/tmp/opencode/target-e2e/debug/team-server \
    TEAM_REQUIRE_PG=1 cargo test
```

`TEAM_REQUIRE_PG=1` makes a skipped database test a failure. Each test creates its own
schema and drops it; nothing touches the homelab `team` database.

Real-CLI tests (omp, OpenCode, Claude Code) never run by default. Only in an isolated
machine or container, with no access to real session stores or credentials:

```sh
HS_E2E=1 sh build-test.sh
```

## Repo map

| Path | What it is |
|---|---|
| `sesync-lock/` | Lock binary: claim/release/check/resolve/sweep/list. State is one file per conversation under `~/.local/share/harness-sync/locks` (default) or `SESYNC_LOCK_DIR`. |
| `sesync-conv/` | Converter binary: omp/Pi journal ⇄ OpenCode/Claude Code. One shared event model, calls and results linked by id. |
| `sesync/` | The daemon, sync pass, and manage TUI. |
| `sesync-team/` | Shared team core (device keys, signed list, sealed records, leases, service) **and** the `team-server` binary. `sesync` depends on it by path, so both sides use one implementation. |
| `hooks/`, `packaging/`, `install.sh`, `install.ps1` | Harness hooks, packaging, installers. |
| `docs/` | Architecture, status, research notes. |

`sesync/src` modules, by role:

- **Local pipeline**: `discovery`, `scan`, `pass`, `install`, `mirror`, `mirror_write`,
  `mirror_cleanup`, `lock`, `titles`, `opencode`, `opencode_rename`, `claude_source`,
  `archive`, `archive_store`.
- **TUI**: `tui`, `app`, `ui`, `keys`, `palette`, `form`, `theme`, `session_cols`, `manage`,
  `qr`, `clipboard`, `remote`, `remote_http`.
- **Team client**: `team_cli` (admin commands), `team_join`, `device_keys`, `sealed`,
  `team_pull_verified` (the live pull), `team_watch`, `team_push`, `team_sync` (trigger),
  `team_pass` (daemon pass), `team_backoff`, `team_import`, `state`.
- **Test-only today** (do not wire new features to these without a decision): `team_pull`
  (superseded by `team_pull_verified`), `team_conflict`, `divergence`.

`sesync-team/src` modules: `membership` (device keys, admin-signed list), `signed_records`
(record format v2), `session_keys` (data keys, wraps, session AEAD), `wrap`, `spaces`,
`auth` (tokens), `totp` (join), `lease`, `service` (rules), `http` (routes), `server`,
`pg` (store), `backup`, `metrics`.

## Invariants that must never regress

Each one has a test; if you change the area, keep the test and add one for the new edge.

| # | Invariant | Enforced in |
|---|---|---|
| 1 | The server never receives or parses session plaintext. | sealed push only, `server.rs`, DB-plaintext tests |
| 2 | One writer per conversation; the write must carry the exact granted lease version. | `lease.rs`, `service.rs::put_sealed_leased` |
| 3 | A record's sequence is its lease version and never goes down; a reader refuses a sequence below its floor. | `signed_records.rs`, `State::team_seen`, `team_pull_verified.rs` |
| 4 | A record is bound to its space, conversation and list version; moving or replaying it breaks the signature. | record v2 header + signature |
| 5 | Trust is anchored at join: the pinned admin key is the only thing that can widen the member list. A fetched list is re-verified on every use. | `sealed::Trust`, `team_join`, `team_watch` |
| 6 | Membership changes are admin-signed and monotonic; revocation applies immediately. | `membership.rs`, `/admin/list`, `/admin/forget` |
| 7 | Only marked sessions are pushed; a session is never pushed by accident. | `team_push::push_marked`, `State::marked` |
| 8 | Mirrors are derived data: never a source, never synced as a conversation. | `State::mirror_keys`, `is_mirror` |
| 9 | The user's chosen mirror targets are the only places the daemon writes; empty means nowhere. | `Source::chosen_targets`, `mirror_write.rs` |
| 10 | Local writes happen only while holding the conversation lock; a locked conversation is skipped. | `lock.rs`, `mirror_write.rs` |
| 11 | The canonical team copy is always the omp/Pi journal format, whatever harness it came from. | `team_sync::canonical_for_source` |
| 12 | A team push is considered only after the source is quiet: 20 s local, 120 s team (configurable). | `scan.rs`, `team_sync::is_due` |
| 13 | The change check hashes the plaintext, never the sealed bytes (sealing is randomized). | `team_sync::push_due` |
| 14 | Device keys, credentials and the admin key are owner-only files (`0600`); the admin private key never leaves its machine. | `device_keys.rs`, `team_join.rs`, `team_cli.rs` |

## Change recipes

**A team wire or record-format change** (highest risk: both sides must agree)

1. Change the shared crate (`sesync-team`) first; it is the single implementation.
2. If the record layout changes, do not edit magic `SR2` in place: use a new magic and keep
   the old-format refusal explicit. There is no production data to migrate, so a clean
   refusal is the migration.
3. Update the server route/handler and the client in the same commit-sized change.
4. Rebuild `team-server` (see above), update the live end-to-end test, then run **both**
   full suites.
5. Update `docs/ARCHITECTURE.md` (wire formats) and `docs/STATUS.md`.

**A new TUI mode**

1. Add a `Mode` variant in `app.rs`; dispatch it in the key handler.
2. Keep handlers pure where possible; terminal I/O stays in `ui.rs` (`draw_*`).
3. Add the draw function and register it in the draw match; keep the footer hint strings.
4. Add a test that drives key events and asserts state, not pixels (`tests/session_menu.rs`
   is the reference).

**A new harness or mirror target**

1. Discovery + scan must learn where its sessions live; mark created files in
   `State::mirror_keys`.
2. Add the converter path and, for team, a `canonical_for_source` arm.
3. Add a writer to `mirror_write::Stores`, add the target to the installed-target probe in
   `bin/sesync.rs::run_tui`, and to the picker filter (never offer a session's own format).
4. Test with a fake CLI executable and the real converter; do not call the real CLI.

**A new environment variable**

1. Read it with a safe default; a bad value must never panic the daemon.
2. Add it to the environment table in `docs/ARCHITECTURE.md`.
3. If it changes behaviour on a timer, add a pure function with tests (see `team_backoff.rs`).

## Handoff discipline

At the end of a work session, update `docs/STATUS.md`: what changed, suite counts, what is
deployed where, what is blocked, and the next action. Keep it under a screen. The next model
should be able to start from `AGENTS.md` → `ARCHITECTURE.md` → `STATUS.md` without reading
the git log.
