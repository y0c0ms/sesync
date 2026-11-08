# Status

Snapshot, not truth. When this file disagrees with the code or the tests, the code and the
tests win, and you fix this file. Update it at the end of every work session. The history of
earlier sessions is in `git log`.

## State on 2026-11-08 (first public release, v0.1.0)

### What works

| Area | State |
|---|---|
| Sync between harnesses | omp, Pi, OpenCode, Claude Code and Antigravity CLI (`agy`) copies kept in step; turn-taking moves the source to the harness answered in; bytes sesync did not write are archived before a copy is replaced. |
| Lock | `sesync-lock` claims per turn through the omp/Pi extension, the Claude Code hook and the OpenCode plugin; agy has no hook (the daemon still claims for its own writes). |
| TUI (`sesync --tui`) | Sessions, Team and Machines tabs; command palette (ctrl+p, also the inbox folder for received files); share picker; divergence dialog with five choices; "Open in herdr" (`o`). Runs as a herdr popup too. |
| Teams | Sealed records on a team server (`sesync-team`, Postgres); join by 6-digit code, admin approval by fingerprint, revoke, leave. A behind copy is caught up by "take theirs" without writing a fork. |
| Phone bridge | `sesync bridge serve` speaks the Agent Hub app's (Orca-based) protocol: conversation list, transcripts, live terminal, prompts, approvals, images, file transfer, dictation (optional whisper server), ntfy push (optional), plan usage. Settings in `<bridge dir>/settings.json`. |
| Machines | `sesync machine add/list/attach/prompt/read/send/get/endpoint/remove`: one machine drives another's conversations through its bridge, once the phone (or a manual pairing) grants it. |

### Tests (2026-11-08, Linux)

- `sh build-test.sh`: 1504 passed, 0 failed, 2 ignored.
- Live team acceptance suite (`TEAM_REQUIRE_PG=1`, real Postgres and team-server): 1174 passed, 0 failed.
- CI (`.github/workflows/ci.yml`) builds and tests Linux, macOS and Windows on pushes to `main` and on pull requests.

### Not verified

- Windows and macOS outside CI: the login-start paths, attach in Windows Terminal, and machine prompts from a Windows host.
- A real phone for the newest bridge methods (machine prompts are exercised through the bridge protocol in `tests/bridge.rs` with a sandboxed herdr and omp on a fake model).
- Real Claude, OpenCode or agy resumes of a context-fitted copy (no real CLI runs in the suite).

### Open decisions

- An agy lock hook, once agy exposes turn events.
