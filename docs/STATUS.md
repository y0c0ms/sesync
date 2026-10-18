# Status

Snapshot, not truth. When this file disagrees with the code or the tests, the code and the
tests win, and you fix this file. Update it at the end of every work session.

## 2026-10-18 — branch `session-keys`

- HEAD `82bdcbc`. The working tree carries a large amount of uncommitted work: client trust,
  record v2, compression, incremental pull, team push trigger, per-session menu, sessions
  table columns. Nothing was committed or pushed; do not assume HEAD describes the tree.
- Suites, last run with the live team environment:
  - `sesync`: **427 passed, 0 failed**
  - `sesync-team`: **199 passed, 0 failed**
  - live env used: `TEAM_TEST_PG=postgres://postgres:test@127.0.0.1:55437/postgres`,
    `TEAM_SERVER_BIN=/tmp/opencode/target-e2e/debug/team-server`, `TEAM_REQUIRE_PG=1`.
- Orientation layer added: `AGENTS.md` (rules, test commands, invariants, recipes),
  `docs/ARCHITECTURE.md` (design, wire formats, routes, persistence), this file, grouped
  module lists in both `lib.rs` files, and an updated README layout.
- What just landed (all in the working tree):
  - **One team config dir**: join, renew, the admin commands, the TUI and the daemon all use
    `SESYNC_CONFIG_DIR` (default `~/.config/sesync`). Before this, `team join` wrote where the
    daemon did not look.
  - **Trust**: pinned admin key + signed list stored at join; every seal/open re-verifies;
    `/admin/devices` and `/admin/forget` added; publish signs the enrolled devices; forget
    kills the token immediately.
  - **Record v2 (`SR2`)**: author + list version + sequence + space + conversation are signed;
    sequence is the lease version; body AEAD is bound to the same `(space, conv, seq)`; v1 is
    refused. The server requires the record sequence to equal the granted lease version.
  - **Rollback floors**: `State::team_seen` records the highest accepted sequence per
    conversation and only rises; a lower record is dropped as rolled back.
  - **Lease fixes**: versions survive release; after a restart the counter continues from the
    stored record's sequence.
  - **Compression**: zstd before encryption with a codec byte, stored when smaller, 64 MiB cap.
  - **Incremental pull**: `/sessions` advertises record sequences; the client skips a fetch
    only when its persisted floor already covers it.
  - **Team loop**: marked sessions, 120 s quiet (`SESYNC_TEAM_QUIET`), canonical omp journal
    for every harness (OpenCode included), plaintext hash idempotency, poll backoff to 60 s.
  - **Mirroring**: `chosen_targets` (empty means no writes), one writer (`mirror_write`),
    per-session menu (Rename / Mark / Mirror / Delete) and a multi-select harness picker.
  - **Sessions table**: TEAM / LAST SYNC / NEXT / MIRRORS / UPDATED columns; OpenCode titles
    now come from the scan-recorded title.
  - **Coverage**: a live test now runs the real `sesync --watch` loop (push, then pull and
    floor raise), and a PTY test drives Enter → session menu → mirror picker → confirm.

## Deployment

- Homelab `team-server` (namespace `team`, ArgoCD app `team-server`) still runs image
  `192.168.100.178:30950/yocoms/team-server:c187d1c` — **older than the working tree**.
- An earlier image `team-server:trust-202610172027-dirty` was built and pushed, but it
  predates record v2 and the Phase 3 changes. **Rebuild and push again before deploying**,
  then bump `~/hlgit/apps/team-server/server.yaml`, update the CHANGELOG + knowledge concept,
  and sync ArgoCD. The production DB is empty; no migration is needed, and old-format records
  (none exist) would be refused cleanly.
- The installed `~/.local/bin/sesync` predates this work and must be rebuilt/reinstalled
  before the local daemon can use the new format.

## Blocked / waiting

- Commit split: the uncommitted work needs an operator-approved set of commits before a
  deploy tag can point at a real revision.
- Plaintext `/record` and `/record/read` routes still exist for legacy tests
  (`http_flow`, `large_session`, `socket`, `push_e2e`); clients no longer use them.
- The live team suite is not in CI (needs Postgres + a built `team-server`).
- `team-backup` timer still needs approval; `sesync.service` disable decision is open.
- Share/retract UI actions are still disabled (no server path yet).

## Structural debt (next model, read this)

- `sesync/src/bin/sesync.rs` (~740 lines) mixes argument dispatch, environment loading, the
  daemon loop, the TUI launcher and the team CLI. Split it into focused modules when it is
  next touched; do not grow it.
- `sesync/src/app.rs` (~1060 lines) holds every TUI mode and handler. New modes should go in
  their own module once they need more than a screen of code.
- `team_pull`, `team_conflict` and `divergence` are test-only; delete them or revive them
  deliberately, but do not add callers without a decision.
- The plaintext `/record` routes exist only for old tests; remove them together with the test
  rework, not piecemeal.

## Next action

1. Build the release binaries, install `sesync`, and re-run both suites once more.
2. Rebuild and push the `team-server` image from the current tree, bump the tag, and roll out
   via ArgoCD; verify `/healthz` and that the admin routes exist, without creating data.
3. Then return to the pending operator decisions above.
