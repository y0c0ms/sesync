# AGENTS.md

How to work in this repo without breaking it. Read this first, then
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/STATUS.md`](docs/STATUS.md) for the last known state. STATUS is a snapshot, not truth:
when it disagrees with the code or the tests, the code and the tests win, and you fix STATUS.

## What this is

DoppelClanker keeps one conversation in sync across harnesses (omp, Pi, Claude Code,
OpenCode, Antigravity CLI) on one machine, and across machines for a team. The team server stores sealed
records only; it can check who wrote what and refuse a write, but it cannot read a session.
One harness writes a conversation at a time, enforced by a lock.

## Hard safety rules

1. **Never run a real harness CLI** (omp, Pi, OpenCode, Claude Code, Antigravity `agy`) as part of
   development or testing, except in a throwaway container with `HS_E2E=1`. Tests that do
   run CLIs are opt-in for that reason.
2. **Never read or write the operator's real session stores**: `~/.omp`, `~/.pi`,
   `~/.claude`, `~/.gemini` (Antigravity's store lives there), `~/.local/share/opencode`, or anything under `~/.local/share/sesync`.
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
| `sesync-conv/` | Converter binary: omp/Pi journal ⇄ OpenCode/Claude Code/Antigravity. One shared event model, calls and results linked by id. |
| `sesync/` | The daemon, sync pass, and manage TUI. |
| `sesync-team/` | Shared team core (device keys, signed list, sealed records, leases, service) **and** the `team-server` binary. `sesync` depends on it by path, so both sides use one implementation. |
| `hooks/`, `packaging/`, `install.sh`, `install.ps1` | Harness hooks, packaging, installers. |
| `docs/` | Architecture, status, research notes. |

`sesync/src` modules, by role:

- **Harnesses**: `harness` holds the `Harness` trait and its registry (`ALL`, `get`), one module
  per harness (`omp`, `pi`, `claude` with `compaction`, `opencode` with `store` and `rename`,
  `antigravity` with `store`), the JSONL helpers they share (`journal`) and the retired ones
  (`retired`). Nothing outside `harness` names a harness.
- **Local pipeline**: `scan`, `pass`, `mirror`, `mirror_write`, `mirror_cleanup`, `turn`,
  `prompts`, `lock`, `archive`, `archive_store`.
- **TUI**: `tui`, `app`, `ui`, `keys`, `palette`, `form`, `theme`, `session_cols` (columns,
  `display_safe`), `manage`, `members` (members dialog state), `team_actions` (the network work
  behind the team dialogs, run on worker threads), `qr`, `clipboard`, `remote`, `remote_http`.
- **Team client**: `team_cli` (admin commands), `team_join`, `device_keys`, `sealed`,
  `team_pull_verified` (the live pull), `team_watch`, `team_push`, `team_sync` (trigger),
  `team_pass` (daemon pass), `team_backoff`, `team_import`, `state`.
- **Test-only today** (do not wire new features to these without a decision): `team_pull`
  (superseded by `team_pull_verified`), `team_conflict`, `divergence`,
  `harness::claude::compaction`.

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
| 5 | Trust is anchored at join: the pinned admin key is the only thing that can widen the member list. A fetched list is re-verified on every use. A join waiting for the admin holds the pinned key and no list. | `sealed::Trust`, `team_join`, `team_watch` |
| 6 | Only the admin admits: the admin-signed list is the roster, and every new version is built on the admin's machine from the list it verified, plus approved keys, minus revoked ones. Nothing joins by being enrolled; a new key waits as a request and its token opens no member route. | `team_cli` (approve/revoke/remove), `Service::enrol`, `Service::authenticate`, `tests/membership_admission.rs`, `team_lifecycle_e2e.rs` |
| 6a | The team code admits nothing by itself: it asks to join or re-authenticates a listed key bound to that machine. A join is signed by the private key of the entry it presents, checked before the code. A listed machine's key is never replaced through a join, a revoked or declined key is refused at every join, and a removed member is refused until invited again. | `Service::enrol`, `http::enrol` (`membership::enrol_message`), `Service::publish_trust_list`, tombstones |
| 6b | List versions only increase, and storing one applies it at once: dropped keys are tombstoned and their machines unbound. | `Service::publish_trust_list`, `/admin/list` |
| 6c | The roster stays inside the team: names, machines, requests and fingerprints only on `/admin/roster`; a member gets the signed key list on `/list`; a pending or revoked device gets no list, no ids, no records. | `http::roster`, `Service::authenticate` |
| 7 | Only marked sessions are pushed; a session is never pushed by accident. | `team_push::push_marked`, `State::marked` |
| 8 | Mirrors are derived data: never a source, never synced as a conversation. A copy recorded on any source counts, OpenCode ids included. | `State::mirror_keys`, `State::is_recorded_mirror`, `is_mirror` |
| 9 | The user's chosen mirror targets are the only places the daemon writes; empty means nowhere. | `Source::chosen_targets`, `mirror_write.rs` |
| 10 | Local writes happen only while holding the conversation lock; a locked conversation is skipped. Every copy's id aliases to the conversation's stable id, so a turn in any copy and the write share one lock. | `lock.rs`, `lock::write_alias`, `mirror_write.rs` |
| 11 | The canonical team copy is always the omp/Pi journal format, whatever harness it came from. | `team_sync::canonical_for_source` |
| 12 | A team push is considered only after the source is quiet: 20 s local, 120 s team (configurable). | `scan.rs`, `team_sync::is_due` |
| 13 | The change check hashes the plaintext, never the sealed bytes (sealing is randomized). | `team_sync::push_due` |
| 14 | Device keys, credentials and the admin key are owner-only files (`0600`); the admin private key never leaves its machine. | `device_keys.rs`, `team_join.rs`, `team_cli.rs` |
| 15 | A conversation's source moves only on a user prompt in a copy newer than every prompt of the source; changed bytes alone never promote. | `turn.rs`, `prompts.rs`, `Harness::newest_prompt` (each harness's prompt reader) |
| 16 | Bytes sesync did not write are never replaced or deleted without a verified archive copy; an archive failure refuses the write. An empty conversation never replaces a copy. | `mirror_write::preserve_foreign`, `archive_store::keep_before_replace`, `scan::journal_has_messages` |
| 17 | A merge never brings back an entry the daemon removed: the daemon adopts only keys it never held, the TUI saves only entries it created. | `State::seen_keys`, `State::save_user`, `App::absorb_daemon_state` |
| 18 | An OpenCode replace is proven importable in a throwaway store before the real session is deleted. | `harness::opencode::store::install_opencode` |
| 19 | A copy is all of its files: an Antigravity conversation is `<id>.db` plus `<id>.db-wal` (agy's live turns sit in the WAL). Change detection, fingerprints, archives and removal cover both, and sesync never creates `-wal`/`-shm` next to agy's file. | `mirror::copy_files`/`copy_hash`/`copy_stamp`, `Harness::files_of`, `harness::antigravity::store::read_only` |
| 20 | A retired harness (Gemini CLI) is only released, never deleted: its sources move to a surviving copy with the same conversation id, its targets and copies are dropped from the records, its files stay on disk. | `State::retire_harness`, `harness::RETIRED`, `bin/sesync.rs::retire_harnesses` |
| 21 | No glyph whose width terminals disagree on reaches the screen (emoji, variation selectors, private-use icons): session text is drawn through `display_safe`, and the finished frame is scrubbed, so one title cannot shift or garble the rest of the screen. | `session_cols::display_safe`, `session_cols::is_unsafe_glyph`, `ui::scrub_unsafe_glyphs` |
| 22 | The TUI never blocks on the network: team calls run on worker threads and their answers are applied between frames. | `team_actions`, `bin/sesync.rs::TeamJobs`, `request_team_pull` |

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

**A membership change** (who may join, approve, revoke): the diagrams and the rules in
`docs/ARCHITECTURE.md` "Team membership and revocation" are the contract. Change them in the same
change as the membership code (`service.rs` enrol/settle/publish, `team_cli` roster commands,
`team_join` waiting), or do not change the behaviour.

**A new TUI mode**

1. Add a `Mode` variant in `app.rs`; dispatch it in the key handler.
2. Keep handlers pure where possible; terminal I/O stays in `ui.rs` (`draw_*`).
3. Add the draw function and register it in the draw match; keep the footer hint strings.
4. Add a test that drives key events and asserts state, not pixels (`tests/session_menu.rs`
   is the reference).

**A new harness or mirror target**

Prerequisite: `sesync-conv` converts it (`from-<id>` into the canonical journal, `to-<id>` back,
and a retitle verb if its title lives outside the conversation file). sesync then needs one
module and nothing else:

1. Write `sesync/src/harness/<id>.rs`, or `harness/<id>/mod.rs` when it carries its own store or
   CLI code, implementing `Harness`. Required: `id`, `icon`, `locate` (its own environment
   variables, with defaults), `present`, `newest_prompt` (turn-taking), `to_journal`, `id_in`,
   `install`, `rename`. A file harness adds `sessions` and `title`, plus `files_of`/`side_files`
   when a conversation is more than one file and `placed_by_cwd`/`misplaced` when copies are
   filed by project. A database harness returns `Kind::Store` and adds `rows`, `row_title`,
   `fingerprint`, `export`, `remove`, `copy_exists`. `harness/journal.rs` has the JSONL helpers.
2. Declare it in `harness/mod.rs` and list it in `ALL` (the picker's order).
3. Test its own reads and writes under `sesync/tests/` with a fake CLI and the real converter;
   never the real CLI.

The scan, turn-taking, mirror writes and cleanup, the team push, and the TUI's list, picker,
titles, renames and icons all come from the registry.

**Retiring a harness** (as Gemini CLI on 2026-06-18): delete its module and its `ALL` entry,
and add a `Retired` entry (its id and the shape of its conversation files) to
`harness::RETIRED`. Every load and merge already releases what an older run recorded for it
(`State::retire_harness`). Never delete its files.

**A new environment variable**

1. Read it with a safe default; a bad value must never panic the daemon.
2. Add it to the environment table in `docs/ARCHITECTURE.md`.
3. If it changes behaviour on a timer, add a pure function with tests (see `team_backoff.rs`).

## Handoff discipline

At the end of a work session, update `docs/STATUS.md`: what changed, suite counts, what is
deployed where, what is blocked, and the next action. Keep it under a screen. The next model
should be able to start from `AGENTS.md` → `ARCHITECTURE.md` → `STATUS.md` without reading
the git log.
