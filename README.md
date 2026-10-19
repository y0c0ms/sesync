# DoppelClanker

Keep several AI coding harnesses working on the same conversation.

You can start a conversation in oh-my-pi (omp), continue it in Pi, pick it up in OpenCode, and then resume it in Claude Code. DoppelClanker keeps each harness's copy of the conversation in step with the others, and makes sure only one harness writes to a conversation at a time.

## How it works

Three parts cooperate:

1. **A canonical journal.** omp and Pi session files are the source of truth. Every other harness gets a converted copy, called a *mirror*. Mirrors are derived data: deleting one and letting the daemon rebuild it is safe.
2. **A converter (`sesync-conv`, Rust).** Converts between the omp/Pi journal format and the OpenCode and Claude Code formats. Images are inlined when moved to Pi. Reasoning content is kept for omp, Pi and OpenCode, and dropped for Claude Code, which signs its reasoning blocks and cannot accept ours.
3. **A lock (`sesync-lock`, Rust) and a sync daemon (`sesync`, Rust).** A conversation is locked while a harness is taking a turn. The lock is claimed when the turn starts and released when it ends, so an idle conversation holds nothing. The daemon rebuilds mirrors when their sources change and skips any conversation that is currently locked.

The lock is enforced inside each harness:

| Harness | Hook |
|---|---|
| omp and Pi | `hooks/omp-lock-shim.ts`, a thin omp extension that forwards events to `sesync-lock hook`. Claims on `turn_start`, releases on `turn_end` and session lifecycle events. |
| Claude Code | `hooks/claude-hook.sh`, wired to `UserPromptSubmit` (claim, block on refusal) and `Stop` / `SessionEnd` (release). |
| OpenCode | A plugin that claims on `chat.message` and releases on `session.idle` and `session.deleted`. |

A refused claim blocks the prompt and names the holder, so two harnesses never write the same conversation at once.

## Layout

```
sesync-lock/   sesync-lock: claim / release / check / resolve / sweep / list
sesync-conv/   sesync-conv: inspect / ids / to-pi / to-omp / to-opencode / to-claude / to-gemini
sesync/        the sync daemon and manage TUI
sesync-team/   team sync core (sealed records, leases, member list) and team-server
hooks/         per-harness lock hooks (omp+Pi extension, Claude Code shell hook)
docs/          architecture, status, research notes
```

## Documentation

- [AGENTS.md](AGENTS.md) — how to work in this repo: safety rules, build and test commands,
  module map, invariants.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — design, trust model, wire formats, routes,
  persistence.
- [docs/STATUS.md](docs/STATUS.md) — last known state, deployment, open decisions.

## Requirements

- Rust (stable) to build `sesync-lock` and `sesync-conv`.
- The harnesses you want to sync: omp, Pi, OpenCode, Claude Code, Gemini CLI.

## Build

```sh
cargo build --release --manifest-path sesync-lock/Cargo.toml
cargo build --release --manifest-path sesync-conv/Cargo.toml
```

Copy the two binaries somewhere on your `PATH` (for example `~/bin/`).

## Configuration

The daemon reads these environment variables. All are optional.

| Variable | Default | Purpose |
|---|---|---|
| `SESYNC_DIR` | `~/.local/share/sesync` | State, locks, logs |
| `OMP_AGENT_DIR` | `~/.omp/agent` | omp data |
| `PI_AGENT_DIR` | `~/.pi/agent` | Pi data |
| `OPENCODE_DB` | `~/.local/share/opencode/opencode.db` | OpenCode database |
| `SESYNC_CONV` | `~/bin/sesync-conv.exe` | Path to the converter |
| `SESYNC_CONFIG_DIR` | `~/.config/sesync` | Team credentials, device key and admin key |
| `SESYNC_TEAM_URL` | unset | Team server; team sync is off without it |

The full table, including the team trigger and poll settings, is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Usage

```sh
sesync --once      # run one sync pass and exit
sesync --watch     # run continuously (SESYNC_POLL seconds apart)
sesync --status    # show current state
sesync --tui       # manage sessions: list, rename, delete
```

Team commands (create/publish/forget additionally need `SESYNC_ADMIN_TOKEN_FILE`):

```sh
sesync team create <name> [--member <m>]
sesync team join <name> --member <m>
sesync team renew <name>
sesync team publish <name>
sesync team forget <name> <machine>
```

Inspect the lock and converter directly:

```sh
sesync-lock list                    # held locks
sesync-lock check <session-id>      # exit 0 if free, non-zero if held
sesync-conv inspect <file.jsonl>    # summarise a journal
sesync-conv ids <file.jsonl>        # list session ids in a journal
```

## Caveats

- **Platform status.** CI builds and tests on Linux, macOS and Windows on every pull request. The Windows and macOS login-start paths have only run in CI, not in a real login session yet.
- **Claude Code sources** are converted by `sesync-conv` (`from-claude` and `to-claude`). The Python session-migrate dependency is no longer used.
- **A crashed harness can leave a lock.** Locks record the harness pid and are swept when the daemon starts. Run `sesync-lock sweep` to clear stale locks manually.
- **Lock failure policy.** If `sesync-lock` is missing, the hooks fail open with a notice. The daemon's divergence guard is the backstop.

## Running the tests safely

`sh build-test.sh` and plain `cargo test` never start a real harness CLI (omp, OpenCode,
Claude Code) and never open your real session stores; every test uses a temporary sandbox.

Tests that drive real CLIs are opt-in. Run them only on an isolated machine or container,
with no access to your real session stores or model credentials:

```sh
HS_E2E=1 sh build-test.sh                         # also run tests that start real CLIs
HS_E2E=1 HS_REQUIRE_CLAUDE=1 cargo test --manifest-path sesync/Cargo.toml --test claude_resume
```

## Acknowledgements

Session conversion in `sesync-conv` is inspired by
[session-migrate](https://github.com/xhluca/session-migrate) by Xing Han Lu (xhluca), MIT licence.
The ideas taken from it are:

- one shared event model (message, tool call, tool result, thinking, compaction) that every
  harness is read into and written out of, with calls and results linked by id;
- the native tool-call and thinking shapes it documents for Claude Code, Pi, omp and OpenCode;
- its conversion rules: pair every tool result with its call, never move signed thinking, count
  every dropped item, and never modify the source session.

No session-migrate code is included in DoppelClanker; `sesync-conv` is a separate Rust
implementation. Research notes are in [docs/research/session-fidelity.md](docs/research/session-fidelity.md).

## License

MIT. See [LICENSE](LICENSE).
