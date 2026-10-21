# DoppelClanker

Keep several AI coding harnesses working on the same conversation.

You can start a conversation in oh-my-pi (omp), continue it in Pi, pick it up in OpenCode, and then resume it in Claude Code. DoppelClanker keeps each harness's copy of the conversation in step with the others, and makes sure only one harness writes to a conversation at a time.

## How it works

Three parts cooperate (diagrams of the whole flow, with any harnesses X, Y and Z:
[docs/ARCHITECTURE.md, How the main logic works](docs/ARCHITECTURE.md#how-the-main-logic-works)):

1. **One source, copies elsewhere.** A conversation's source is the harness where it is live; every harness the user chose for it gets a converted copy, called a *mirror*. Answering inside a copy moves the source there (turn-taking), and the other copies follow. Mirrors are derived data, and bytes sesync did not write are archived before anything replaces them.
2. **A converter (`sesync-conv`, Rust).** The omp/Pi journal is the interchange format; the converter reads and writes the OpenCode, Claude Code and Antigravity formats through it. Images are inlined when moved to Pi. Reasoning content is kept for omp, Pi, OpenCode and Antigravity, and dropped for Claude Code, which signs its reasoning blocks and cannot accept ours.
3. **A lock (`sesync-lock`, Rust) and a sync daemon (`sesync`, Rust).** A conversation is locked while a harness is taking a turn. The lock is claimed when the turn starts and released when it ends, so an idle conversation holds nothing. The daemon rebuilds mirrors when their sources change and skips any conversation that is currently locked.

The lock is enforced inside each harness:

| Harness | Hook |
|---|---|
| omp and Pi | `hooks/omp-lock-shim.ts`, a thin omp extension that forwards events to `sesync-lock hook`. Claims on `turn_start`, releases on `turn_end` and session lifecycle events. |
| Claude Code | `hooks/claude-hook.sh`, wired to `UserPromptSubmit` (claim, block on refusal) and `Stop` / `SessionEnd` (release). |
| OpenCode | A plugin that claims on `chat.message` and releases on `session.idle` and `session.deleted`. |
| Antigravity CLI (`agy`) | None yet (sesync ships no agy hook). The daemon still claims the conversation's lock for every write, refuses to refresh a copy agy holds open (retried later), and archives any agy turn it would replace. |

A refused claim blocks the prompt and names the holder, so two harnesses never write the same conversation at once.

## Layout

```
sesync-lock/   sesync-lock: claim / release / check / resolve / sweep / list
sesync-conv/   sesync-conv: inspect / ids / to-pi / to-omp / to-opencode / to-claude / to-antigravity / from-*
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
- The harnesses you want to sync: omp, Pi, OpenCode, Claude Code, Antigravity CLI (`agy`).
  Gemini CLI was retired on 2026-06-18 in favour of Antigravity: sesync moves Gemini-era records
  to a surviving copy on its first start and never touches a Gemini file.

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
| `ANTIGRAVITY_CLI_HOME` | `~/.gemini/antigravity-cli` | Antigravity CLI store (`conversations/<id>.db`) |
| `AGY_BIN` | `%LOCALAPPDATA%\agy\bin\agy.exe`, else `agy` on PATH | Only decides whether Antigravity is offered as a target; never run |
| `SESYNC_CONV` | `~/bin/sesync-conv.exe` | Path to the converter |
| `SESYNC_CONFIG_DIR` | `~/.config/sesync` | Team credentials, device key and admin key |
| `SESYNC_TEAM_URL` | unset | Team server for `team create`/`join`; a joined team remembers its own server |

The full table, including the team trigger and poll settings, is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Usage

```sh
sesync --once      # run one sync pass and exit
sesync --watch     # run continuously (SESYNC_POLL seconds apart)
sesync --status    # show current state
sesync --tui       # manage sessions: list, rename, delete
```

The TUI lists sessions with TITLE, MIRRORS, SYNCED, TEAM and UPDATED columns, the harness shown as
omp, pi, claude, opencode or agy. MIRRORS shows each chosen copy: `pi` once it is written, `pi?`
while it waits for its first write, `pi~` while the conversation is open in that harness (the
daemon retries by itself), and `pi!` when its last write failed. SYNCED shows the age of the oldest
good write to every chosen copy (`ok` until that time is known), or `waiting` or `failing`. On a
narrow terminal the columns go in this order: TEAM, UPDATED, MIRRORS, SYNCED. At 120 columns or
more a details pane shows the selected session and, per copy, when it was last written or why it
failed. The tab bar says when the daemon last ran a pass and how many copies are in place or
failing. A session whose file is gone is listed by its name with `(missing)`. Emoji and icon glyphs
in titles are left out of the display, because terminals draw them at different widths; the stored
title is unchanged. The mouse selects a row, the wheel scrolls, and a double-click works like Enter.

Team commands. `join` and `renew` run on a member's machine; every other command runs on the
admin's machine (the one that created the team, which holds the admin key) and also needs
`SESYNC_ADMIN_TOKEN_FILE`:

```sh
sesync team create <name> [--member <m>]
sesync team invite <name> <member>
sesync team join <name> --member <m>        # a new device waits for the admin; prints its fingerprint
sesync team renew <name>
sesync team roster <name>                   # listed devices and waiting requests, with fingerprints
sesync team approve <name> <member> <machine> [--fingerprint <fp>]
sesync team decline <name> <member> <machine>
sesync team revoke <name> <member> <machine>
sesync team remove <name> <member>          # every device of the member; refused until invited again
```

Only the admin admits a device: a join with the team code asks, and `approve` (after comparing the
fingerprint the member reads from their own `join`) signs the device into the member list. The
daemon starts syncing on its own once the device is approved. `revoke` and `remove` take effect at
once and the revoked key is refused at every later join, so nobody re-scans the QR when someone
leaves.

The same work is available in the TUI (`sesync --tui`) on the command list (ctrl+p), with no
environment needed. "Start a team" asks for the team, its members, the admin token file and the
server, then shows the one-time authenticator QR on a screen of its own, black on white, with the
setup key below it for typing. An 80 by 24 terminal holds the code; a smaller one says the size it
needs and shows the key. With `NO_COLOR` set the code is drawn in the terminal's own colours with
the light modules painted, which reads the right way round on a dark terminal. The machine that
started a team remembers it (team, server and token file path in `config/admin/teams.json`), so
"Team members" and "Invite a member" work there from then on, before it joins the team itself.
"Join a team" asks for the server, the team, your member name, this machine's name and the 6-digit
code, and shows the fingerprint to read to the admin. The Team tab shows, per team, where this
machine stands: listed, or waiting for the admin with its fingerprint.

In the members dialog, `a` approves the selected waiting request, `d` refuses it, `r` revokes a
listed device and `m` removes a member. Every change takes two presses on the same row; moving the
cursor or pressing another key cancels it. Approval sends the fingerprint the member read, so the
server refuses a request that changed. The admin's own key cannot be revoked or removed from the
dialog. `t` switches between teams when more than one is configured.

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
