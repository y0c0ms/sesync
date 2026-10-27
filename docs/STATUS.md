# Status

Snapshot, not truth. When this file disagrees with the code or the tests, the code and the
tests win, and you fix this file. Update it at the end of every work session.

## 2026-10-27: the TUI opens without reading every conversation, and Esc closes it

- **Why.** On the work PC the herdr popup (ctrl+alt+s, `sesync --tui`) stayed blank for seconds and
  Esc did nothing. Measured with `SESYNC_TRACE` (new milestones: loading drawn, harnesses detected,
  preview worker started, app built, team and prefs loaded, drawn): `App::new` took 2.2 to 4.1 s,
  all of it reading the titles of 132 journals (about 880 MB of omp and Claude files) before the
  first frame. The preview already read only the highlighted session; the titles did not.
- **Titles off the startup path** (`manage::list_groups_deferred`, `title_worker::Worker`,
  `manage::TitleCache::{load, save}`, `App::new_deferred`). Listing reads folder entries, sizes
  and change times only. A row shows the title the previous run saved (`titles.json` beside
  `state.json`, valid while size and change time match), else the last title read, else the
  daemon's record, else "untitled"; the worker reads the rest, rows on screen first, and the cache
  is saved when it is idle and on quit. `App::new` (CLI, tests) still reads titles on the spot.
  ARCHITECTURE "What the TUI reads, and when" has the sequence.
- **Cheaper title reads** (`journal::read_title_text`, `omp_title_from_lines`,
  `claude_title_from_lines`). The tail walk prepended each 64 KiB chunk to the text read so far,
  re-copying up to 8 MiB at every step; chunks are now joined once. Both rules skip, before
  parsing, every line that cannot hold a record they still need. Same results.
- **Loading screen.** "Reading sessions…" is drawn before the list is built.
- **Esc.** In the lists Esc now clears a kept filter, else quits; on an armed delete it only
  cancels the delete; in a dialog or overlay it closes that, as before. The `?` help lists
  `q / esc`; the footer keeps `q quit` (a longer hint pushed "quit" off a 100-column screen).
- **Tests.** `tests/title_worker.rs` (the deferred list opens no conversation and asks for rows in
  the order shown, once; the daemon's name stands in; a saved cache names unchanged files and only
  changed ones are read; an unreadable cache is an empty one; the worker answers in order),
  `tests/app.rs` (four Esc cases), `tests/title_long_journals.rs` (names of 7 to 12 MiB realistic
  journals: header, deep rename, first prompt, Claude tool results never a prompt; a mutant
  prefilter fails it), `tests/tui_pty.rs` draws the loading line before the list and quits on q
  and on a lone Esc byte in a real pty (POSIX).

## 2026-10-27: a list change no longer hides every shared conversation

- **Why.** Found when the work PC joined HomelabTeam (list v3 -> v4): from then on every pull, on
  every machine, dropped the two conversations shared before the join, the authors' own included.
  Readers refuse a record sealed under an older list (`StaleList`), the new member was never a
  recipient, and a push only went out when the content changed. Every approve, revoke or removal did
  the same.
- **Now.** The push step re-seals, under the current list, each unchanged shared conversation whose
  stored record is this machine's own and sealed under an older list (`team_sync::needs_reseal`),
  once per conversation and list version (`TeamPush::list`, kept in the state file). "Own" is proven
  by the record's signature (`signed_records::authorship`, new in sesync-team), with the floor this
  machine accepted as the lowest sequence, so a forged header or a replayed older record cannot make
  a machine republish. A record someone else wrote last is left for them, so an older copy never
  replaces newer content. The daemon logs `N re-sealed under the current member list` and counts it
  as activity for the poll interval.
- **Not fixed.** A machine that is off keeps its records unreadable until it runs again; nobody else
  can re-seal them (only the author signs its records).
- **Suites** (Windows, sandboxed): `sesync` 603 passed, `sesync-team` 199 passed, 0 failed.

## 2026-10-21: team exits, refusals that say so, and files filed where the harness lists them

Found by running a team end to end on this machine (an in-memory team server, five sandboxed
members, each with its own TUI and daemon) and reading the harnesses' own sources.

- **Pull stuck.** The TUI applied a pull's result only on the next key, and a second ctrl+r
  while one ran dropped the running one, so "a team pull is already running" stayed for the rest
  of the run. The result is now collected on every loop turn and a refused request keeps the
  running pull.
- **A refused device read as an empty team.** A revoked or removed device saw "team pull: 0
  verified" and could still share; the listing's 403 was swallowed. The pull now carries the
  server's reason (`PullOutcome::error`): the status line, the team's card and the daemon log say
  "the team server refused this device (403: not a member)", and push failures are logged with
  their reasons, not only counted.
- **No way out.** A declined join waits forever (the server answers a declined key as a pending
  one), and a revoked membership stayed configured. "Leave a team" (command list) removes a
  membership's credential, unshares its sessions and takes the rows only that team's pulls brought
  over off the Team list, or forgets a team managed here; Enter twice confirms. The share picker
  also lists a team a session is still shared with after this machine left it, so the share can
  be untied.
- **The member list never moved.** A credential kept the list from its approval: the card said
  "list v3" at v8, and the rollback guard only measured against the approval-time list. Every
  pull and push step now keeps a newer verified list (`team_join::ratchet_list`).
- **Files no harness lists.** A team import wrote `omp/sessions/<file>`, a level omp never
  reads. Pi copies went to `-<absolute>-`, but Pi (1.1, `getDefaultSessionDirPath`) lists a
  project only from `--<absolute>--`; omp (18.12) puts a cwd in the temp root under
  `-tmp-<relative>` and any other cwd outside the home under `--<absolute>--`. The rules in the
  converter and in `harness/journal.rs` now follow the harnesses' sources (a recorded copy in a
  wrong folder counts as misplaced and is moved on the next pass), and an import goes to the
  folder the harness lists for the session's cwd, named after its start time; an id the harness
  already holds is refused. `sesync` and `sesync-conv` must be installed together.
- **Two TUIs overwrote each other.** A TUI saved its whole set of shares, chosen targets and
  deletes, so with two open the later save erased the earlier one's choices, and neither showed
  the other's until restarted. Each now keeps the choices as it last read or wrote them
  (`State::user_base`): a save writes only what changed in it (a three-way merge per
  conversation and per session), a re-read of the file takes every other choice, and deletes are
  only ever added.
- **Leaving emptied the Team list.** A pulled row did not record its team, so leaving one emptied
  the list until the next ctrl+r. The app now keeps each team's verified copies
  (`TeamWatch::copies`) and builds the list from them: leaving removes only the rows no other team
  holds, and a conversation another team holds too stays, as that team's copy.
- **Pi leftovers.** Three Pi files from 2026-10-12 (copies an older tool wrote, recorded as
  sources) sat in single-dash folders Pi never lists. They were deleted through the TUI, so their
  records went with them, and the eleven empty single-dash folders were removed.
- **CI on Linux and macOS** (PR #56) ran tests Windows never runs. A unix-only TUI test wrote
  the admin token file readable by others (the dialogs refuse that), the pty test waited for the
  Team tab's old text, and the mirror picker showed claude without its icon: U+2736 sits in the
  Dingbats block the screen scrub drops (now U+22C6, and a test keeps every harness icon out of
  the scrubbed ranges). One was a real bug: with `/tmp` as the temp root, a promotion's omp
  rewrite lands in the folder the losing file sits in, the install's same-id sweep removes that
  file (after the archive kept it), and its key stayed in `mirror_keys`; a superseded file
  already gone now leaves no key. On Windows, `TEMP=/tmp` reproduces Linux's temp root: the
  converter and the folder check both read it.
- **Suites** (Windows, sandboxed): `sesync` 598 passed, 0 failed (also with `TEMP=/tmp`);
  `sesync-conv` 75 passed.

## 2026-10-21: a deleted session leaves the list; live OpenCode sessions are not "missing"

- **Why.** A delete in the TUI removed the file only. The record stayed, the list shows a record
  without a file as "(missing)", and nothing could remove it: the daemon owns the records and the
  TUI had no way to tell it, and deleting the "(missing)" row was refused (the path check needs
  the file). Separately, every OpenCode session was flagged "(missing)": the list looked only at
  harness files, and on narrow columns the tag was cut off with the title.
- **Now.** A delete is recorded in the state's user-owned `deleted` (key and time). The TUI, its
  save, and every daemon load and merge (`release_records`) forget each deleted session that is
  gone: the record goes, its copies are released and listed as sessions of their own, and its
  shares go. A deleted session whose file is back is kept; the marker lasts a day. Deleting a
  "(missing)" row forgets it without touching any file. A database harness's session is missing
  only when its database no longer lists it.
- **Suites** (Windows, sandboxed): `sesync` 587 passed, 0 failed.

## 2026-10-21: the details pane previews the last messages; team dialogs show examples; titles

- **Preview.** At 120 columns or more the details pane shows the highlighted session's last 20
  turns, newest at the bottom; shift+up/down or the wheel over the pane scrolls back. A worker
  thread (`preview::Worker`) reads them and checks the session's stamp every second, so a new
  message appears by itself. Each harness reads its own end (`Harness::recent_messages`): omp and
  Pi read the journal, agy goes through `from-antigravity`, Claude reads the transcript's end and
  OpenCode its database (no CLI export). Thinking, tool results, meta and subagent records are
  left out; an assistant answer split over records is one turn, and a long one keeps its newest
  2000 characters.
- **Converter output** is captured in `journal::convert_from` instead of inherited, so nothing
  the converter prints can land on the TUI's screen; a failure now carries its reason.
- **Team dialogs.** Every empty field shows a made-up example (`e.g. design-team`,
  `e.g. ana, rui`) and the focused field says what it is for. Examples are never values, so
  Enter on an untouched "Start a team" is refused ("'team' is empty"). "members" are the people
  allowed to join; the admin lists themselves too to sync their own machine.
- **Titles.** An omp/Pi title is now the newest name record by time (the slot's `updatedAt`, a
  rename's `timestamp`), not the last one read. A July rename near the head of the 44 MB "PC
  Config" session overrode the slot rewritten on 10-20 (the newest rename sits 16.7 MB from the
  end, past the 8 MB tail walk), so the list showed the July name and the rename form offered it
  back: the 10-19 overwrite of that title came this way.
- **Suites** (Windows, sandboxed): `sesync` 582 passed, 0 failed.

## 2026-10-21: one module per harness

- **Why.** What sesync knew about each harness was spread over a dozen places (discovery,
  titles, install, buckets, prompts, `mirror_write`, `scan`, `manage`, `team_sync`, the daemon's
  scan and cleanup, the TUI's target probe), so adding or removing a harness meant edits in all of
  them.
- **Now.** `sesync/src/harness/` holds the `Harness` trait, the registry (`ALL`, `get`), one
  module per harness (`omp`, `pi`, `claude/`, `opencode/` with its store and rename,
  `antigravity/` with its store), the shared JSONL helpers (`journal`) and `RETIRED`. The daemon,
  the TUI, mirror writes, cleanup, turn-taking and the team push go through the trait; outside
  `harness/` no production code names a harness. `Roots` and `Stores` carry one `Locations`;
  `retire_gemini` became `retire_harnesses` over `RETIRED`. OpenCode's two default-title rules
  are one now (the stricter one its tests define). `sesync mirrors` now gives a database copy's
  keep reason as "opencode keeps this session in its own store: remove it through opencode".
- **Checked.** On a copy of the crate, deleting `harness/opencode/` and adding a throwaway
  harness (one file, two lines in `harness/mod.rs`) compiled with no other change, and its
  daemon mirrored an omp session into the new harness and recorded the copy.
- **Suites** (Windows, sandboxed): `sesync` 572 passed, `sesync-team` 196, `sesync-conv` 74,
  `sesync-lock` 19; 0 failed in each.
- **Open.** `harness::claude::compaction` has had no caller since conversion moved to
  `sesync-conv from-claude` (dead before this change too): remove it or wire it.

## 2026-10-21: TUI shows per-copy sync state; team setup done entirely in the TUI

- **Bugs fixed.** (1) A title starting with U+2696 + U+FE0F (an emoji presentation: two cells in one terminal,
  one in another) shifted every later column and left stale characters on screen: session text
  goes through `display_safe` and the finished frame is scrubbed (`ui::scrub_unsafe_glyphs`), so
  no such glyph is sent; widths come from `unicode-width`, the table ratatui places glyphs with.
  (2) The harness column is 8 cells for every label (`agy` for Antigravity), and the list's 2-cell
  cursor gutter is counted, so UPDATED is no longer clipped. (3) A missing session shows its
  recorded name, else `untitled`, with `(missing)`; never a file name or a store id.
- **Sync state.** The daemon records each chosen copy's last good write and last failure
  (`Source.sync`); `settle_sync_records` prunes un-chosen copies every pass and backfills the
  write time of file copies made before the record existed (an OpenCode copy waits for its next
  write). MIRRORS marks `?` unwritten, `~` busy (in
  use, retried by itself), `!` failed; SYNCED is the oldest good write, `ok`, `waiting` or
  `failing`. Columns drop on narrow panes in the order TEAM, UPDATED, MIRRORS, SYNCED. At 120
  columns a details pane gives each copy's time or error. The tab bar shows the daemon's last pass
  and copy counts. The mouse selects, scrolls and double-clicks.
- **Team in the TUI.** "Start a team" takes the server in the form (no environment needed) and
  shows the QR full screen, ECC Low on the URI without its default parameters (version 5 for a
  typical name): an 80x24 terminal holds it, with a 4-module quiet zone when there is room and 2
  when that is what fits. Black on white; with `NO_COLOR` the light modules are painted instead,
  which keeps the right polarity on a dark terminal. A team started here is remembered
  (`<config>/admin/teams.json`), so "Team members" and "Invite a member" work before the machine
  joins it. The members dialog approves (two presses, fingerprint shown), refuses, revokes and
  removes. The Team tab shows per team whether this machine is listed or waiting (with its
  fingerprint). Network calls run on worker threads. Em dashes are gone from the UI text.
- **Checked live** (WezTerm, sandboxed stores, a throwaway in-memory team server on 127.0.0.1):
  the QR screenshot decodes with jsQR in strict standard polarity to the URI whose secret is the
  key on screen; a TOTP code computed from the scanned secret let a second machine join (pending,
  fingerprint shown); the admin's members dialog showed that same fingerprint and approved it;
  the member's daemon then reported the approval and synced. The operator confirmed the QR renders
  well.
- **Suites** (Windows, sandboxed): `sesync` 568 passed, 0 failed; `sesync-team` 196 passed, 0
  failed.
- **Open.** The daemon status is the state file's age, so a TUI save also refreshes it. Long
  status messages are cut at the terminal's width.

## 2026-10-21: team revocation sticks; only the admin admits

- **Why.** `team publish` signed every enrolled device, so a forgotten device re-joined with the
  shared code and was signed back in; `/enrol` handed the signed list to any code holder and let
  one replace a listed machine's key; `save_owner` ran before the code was checked.
- **Now** (contract: ARCHITECTURE.md "Team membership and revocation"). A new key gets 202
  pending, a token that opens nothing, the admin key and its fingerprint, and no list; a listed
  key bound to that machine re-authenticates; a new key under a listed machine's name gets 409
  and changes nothing; a revoked or declined key gets 403 at every join; a removed member is
  refused until invited again. Requests: 3 per member, 20 per team, 7-day expiry. Storing a list
  tombstones and unbinds dropped keys and binds approved requests. New admin routes
  `/admin/roster`, `/admin/decline`; `/admin/devices` and `/admin/forget` are gone. CLI:
  `team roster|approve|decline|revoke|remove` (each builds v+1 from the list it verified with
  the admin key); `publish` and `forget` are gone. The daemon and the TUI pick up an approval on
  their own and log "waiting for the admin" once per team per run. Postgres gains `requests` and
  `tombstones` (created by `migrate`). An existing team's unlisted bindings become requests on
  first use; nobody re-scans the QR. `membership::Join`/`admit`/`is_member_at` and the join
  window were test-only and are removed.
- **Suites** (Windows, sandboxed): `sesync-team` 195 passed, 0 failed (Postgres tests skipped:
  no `TEAM_TEST_PG`); `sesync` 538 passed, 0 failed (live team-server tests skipped).
- **Not deployed.** The team server and clients must be upgraded together: an old client reads
  a 202 as a join and saves a credential with no list, and an old client's join (unsigned) is
  refused with "upgrade the client" by a team that has an admin key.
- **Join proves the device key** (closed the same day). A join carries the device's Ed25519
  signature over `membership::enrol_message` (space, member, machine, code, entry), checked
  against the entry's signing key before the code, so a member who knows a listed machine's
  name and public entry gets 403 and no token, and a forged attempt spends no code
  (`device_binding::a_join_must_prove_it_holds_the_device_key`).

## 2026-10-21: sync/mirror audit after a live loop destroyed an OpenCode session

- **Incident.** With the 15e70f4 build the daemon promoted a Gemini copy of the "Record"
  conversation (its state record had fallen behind, no one answered), demoted the OpenCode source,
  re-adopted that source from disk in the same pass, and then promoted the copy every pass. Each
  pass ran `opencode session delete` on the real source and an import that failed (`Missing key
  at [parentID]`): 6926 messages lost. The OpenCode WAL showed 46 deletions, 44 of them on
  10-19 by the older build. Recovery from the WAL got 4409 messages, every part over about 4 KB
  lost; the original omp journal held the whole conversation, so it was restored as the source
  (copies backed up in `~/.local/share/sesync/recovery-20261021T134600/`).
- **Fixed** (each with a test): a source moves only on a user prompt newer than every prompt of
  the source (`prompts.rs`; renames, resume bookkeeping and stale records no longer promote);
  tangles collapse before detection; the daemon adopts from disk only keys it never held
  (`State::seen_keys`) and the TUI saves only entries it created; recorded copies (OpenCode ids
  included) are never sources in the scan or the list; bytes sesync did not write are archived
  and verified before any replace (`preserve_foreign`, `keep_before_replace`), the demoted source
  is recorded as not sesync's and replaced in place under its own id; an OpenCode replace is
  proven in a throwaway store first, deletes where the store keeps the session, imports in the
  conversation's directory; un-choosing OpenCode deletes only a provably unchanged copy and
  releases the rest; an empty conversation never replaces a copy; `to-opencode` gives an
  assistant-first conversation a `msg_` parent; every copy id is aliased to the conversation's
  lock name (`lock::write_alias`) and every write claims that name.
- **Team audit** (16 defects, `tests/team_lifecycle_e2e.rs` on an in-process server): otpauth
  label encoding, QR one column per module with the setup key shown, create validated before the
  name is taken, names with `|` refused, a per-member join lock, explicit join errors, re-join
  restarts the renewal window, automatic token renewal in the daemon and the TUI, encoded push
  query and credential file names, the credential records its server, one damaged credential no
  longer stops other teams, team poll backoff per team, codes typed as "123 456", forgetting an
  unknown machine is an error. Open decision: a forgotten device can re-join and the next
  `publish` signs it back in (secret never rotated, no remove-member, publish lists nothing).
- **Windows test suite** compiles again: Unix-only shell fakes gated with `cfg(unix)`,
  `sesync-lock` paths use the platform exe suffix, JSON-escaped Windows paths in fixtures.
- **Docs**: ARCHITECTURE.md gains "How the main logic works" (mermaid, harnesses as X/Y/Z);
  AGENTS.md invariants 15 to 18.

## 2026-10-21: Antigravity CLI replaces Gemini CLI in the sesync crate

- **Harness `antigravity`** (agy 1.4.4) is a source and a target. Store
  `$ANTIGRAVITY_CLI_HOME` (default `~/.gemini/antigravity-cli`), conversations
  `conversations/<id>.db`. Targets: `sesync-conv to-antigravity` (`install::install_antigravity`;
  "in use" while agy holds the file is a retry). Sources: `sesync-conv from-antigravity` in
  `sync_chosen` and `team_sync::Canonical::Antigravity`. Offered in the picker when agy is
  installed (`AGY_BIN`, else `%LOCALAPPDATA%\agy\bin\agy.exe`, else PATH); sesync never runs it.
- **WAL-aware copies**: `mirror::copy_files/copy_hash/copy_stamp/copy_mtime/remove_copy` cover
  `<id>.db` + `<id>.db-wal` in the scan, turn-taking, `preserve_foreign` and cleanup.
  `antigravity::newest_prompt` reads the newest USER_INPUT step; `antigravity::read_only` never
  leaves `-wal`/`-shm` beside agy's file. Titles: annotations, else agy's cached summary title;
  TUI rename runs `antigravity-retitle` (exit 2 = "applies when agy restarts").
- **Gemini retired**: every Gemini code path and test is gone from the sesync crate.
  `State::retire_harness("gemini", exists)` runs at load and every merge (daemon, `sesync
  mirrors`, TUI): Gemini sources move to their best surviving copy (omp, Pi, Claude, OpenCode,
  Antigravity), keeping conv/title/shares; Gemini choices and copies are released; no Gemini
  file is touched. The Gemini converters (`from-gemini`, `to-gemini`) are removed from sesync-conv.
- **Suite** (Windows, sandboxed HOME, without team_cli/team_join/team_sync/device_keys):
  480 passed, 0 failed. New: `tests/antigravity_target.rs` (7, real converter),
  `tests/retire_gemini.rs` (7), discovery and unit tests.
- **E2E** (research sandbox, local mock): `sesync --once` mirrored an omp journal into the agy
  store, agy resumed it with the history replayed, the agy follow-up promoted the agy copy and
  rewrote omp, and a conversation agy created itself mirrored into omp.
- **Open**: no agy lock hook (agy turns are not excluded by the lock, only by the converter's
  in-use refusal); `from-*` converters print their output path on the daemon's stdout.

## 2026-10-18 (late evening) — TUI quality-of-life batch (same branch)

- Operator approved the QoL list from the TUI audit; all items implemented except the mouse
  wheel (already works) and the two server-side ones left open (below).
- **Filter and scope** (Sessions tab): `/` opens a fuzzy filter typed at the status line — the
  list narrows live (subsequence match, same as the command list, `palette::fuzzy_matches`),
  Enter keeps it, Esc clears it; `f` cycles the scope all → marked → mirrored → failing (a
  marked row vanishes from the "marked" view the moment it is unmarked); footer and status
  show the active scope and `N of M` counts. The list cursor follows the session by key on
  every re-list and keeps its row position after a delete.
- **Mirror picker** no longer pre-ticks every installed harness (one stray Enter used to
  mirror a session everywhere); it starts from the session's saved choice plus its existing
  mirrors, `a`/`n` tick/clear everything, each row shows `mirrored`/`chosen` state, and the
  footer's `a all · n none` promise is now real.
- **Status line**: every message now carries its severity set at the source (`say`/`fail` in
  `app.rs`; no more substring sniffing in `ui.rs`). Informational messages fade after 25 s
  (`app::STATUS_TTL`); failures stay until replaced and draw in the error colour — this also
  fixes form errors ("not joined: …") which were never red. Team pulls run on a worker thread
  (`bin/sesync.rs::request_team_pull` + `apply_pull`), so the terminal stays live; a second
  ctrl+r while one is running says so, a lost worker says "did not finish", and the app's
  `team_seen` floors rise to what a pull accepted (in memory only).
- **Team tab context**: a dim header line — `N session(s) · pulled Xs ago · M refused` — so an
  empty list is not mistaken for an empty team; rows are `title · first8…` instead of the
  full `[uuid]`.
- **Help overlay**: `?` on either tab opens a key list for that tab (`Mode::Help`,
  `app::help_lines`), Esc/enter/`?` closes.
- **Session menu**: gained "Show transcript" (`t`) — the session's own file read inside the
  TUI, scrollable (`j/k`, PgUp/PgDn, Home/End), tail-capped at `TRANSCRIPT_MAX_LINES` (4000)
  lines with a cut marker — and "Copy its path" (`ctrl+c` behaviour from the menu).
- **Delete confirmation**: the second ctrl+d now opens a dialog naming the session and its
  source path, stating that only the source file goes (mirrors and the team's sealed copies
  stay — as `delete_group` has always done), confirmed with ctrl+d/enter/y, cancelled by any
  other key. Renaming it "and its mirrors" in the help was wrong and is fixed.
- **Persistence**: theme + last tab survive runs via `~/.config/sesync/tui.json`
  (`app::prefs::load/save`, written by the TUI loop only when the taste actually changed;
  never in the daemon's `state.json`).
- **Statusline bar** (same evening, operator request): both bottom rows — status and footer —
  sit on a per-theme `bar` colour (`theme.rs`, a strip distinct from the page background, like
  neovim's statusline), so a typed filter or a message is easy to find; while the filter is
  being typed its text is bold foreground. `render.rs` asserts the strip carries the bar
  background beyond the text and that errors keep their colour on it.
- Suites: `sesync` **486 passed, 0 failed** (was 465; +21 tests: `session_filter.rs` ×4,
  `session_menu` +3, `tui_views` +3, `app` +2, `sessions_table` ×2 colour tests,
  `session_cols` +1 and the restructured mirror-state asserts, `render` +1 bar test),
  `sesync-conv` 73/0, `sh build-test.sh` green. Known stale line: `tests/tui_form.rs` module
  doc now corrected.

## 2026-10-18 (late evening) — full-background themes, more schemes (same branch)

- Operator: themes must own the page background like a text-editor scheme, and there should be
  more of them.
- `Theme` gained `bg`: `draw` paints it over the whole frame first, and every other widget
  still only sets foregrounds, so the fill shows through everywhere (rows, tab bar, dialogs —
  `dialog()` restamps its rectangle with `bg` instead of `Clear`, so no list text leaks
  under a floating box). The highlighted row and the bottom bar keep their own explicit
  backgrounds (accent / bar).
- 6 new schemes, all classic editor palettes with real backgrounds: `solarized (dark)`,
  `gruvbox (dark)`, `gruvbox (light)`, `nord`, `dracula`, `catppuccin mocha` — 10 themes
  total; the existing four got matching bg values (textual dark 0x101010, light white,
  tokyo-night 0x1A1B26, solarized light base3). The picker, theme persistence (`tui.json`)
  and live preview needed no changes: previewing now shows the whole scheme, background
  included.
- Tests +2 (`tui_theme.rs`): walking the full picker asserts each previewed theme's bg is on
  the page, and a dialog restamps its area; `names_sorted() >= 10`. Suites: `sesync`
  **488 passed, 0 failed** (was 486), `tui_pty` 3/0, `sh build-test.sh` green.
- Deployed (backup `sesync.prev-themes`), daemon restarted, active.

## 2026-10-18 (late evening) — stale "failing" counters healed, scope fixed (same branch)

- Operator: 22 sessions showed in the `f` → failing view. Diagnosis (metadata only): all 22
  have `chosen_targets: []` and `mirrors: {}` — leftovers of the old auto-mirror binary — and
  only stale `failures` counters (1, 5, one 2770). Nothing is being written for them; a
  session asked to mirror nothing cannot fail anything. The failing *scope predicate*
  (`failures > 0` alone) was wrong; the MIRRORS column markers were already intent-aware.
- Code: `passes`' Failing arm now requires mirror intent too (`failures > 0` AND
  (`chosen_targets` or `mirrors` non-empty)). New test
  `a_stale_failure_counter_without_mirror_intent_is_not_failing`.
- State heal (protocol: stop → backup → patch → start): zeroed `failures` on sources with no
  chosen targets and no mirrors — exactly the 22. Backups
  `/tmp/opencode/state-pre-stale-failures.json`, `state-post-stale-failures.json`. After the
  restart, 0 sources carry failures (the daemon does not re-add them: with no intent there is
  no attempt to fail).
- Suites: `sesync` **489 passed, 0 failed**, `sh build-test.sh` green. Deployed (backup
  `sesync.prev-stalefail`), daemon active.

## 2026-10-18 (night) — turn-taking: an answer in a mirror becomes the source (same branch)

- Reported: a session created on OpenCode ("TestFromOPENCODE") mirrored to claude+omp; the
  operator answered inside both copies, got responses, and then nothing propagated: last sync
  stuck at creation, opencode showed no new turns.
- Cause: the local pass decides entirely from the **source's** hash (`scan` in
  `bin/sesync.rs`); mirrors are one-way derived data. Writes into a mirror never propagate,
  and nothing even re-fingerprints mirrors (`divergence::check_mirror` is test-only), so the
  daemon never noticed the edits.
- Design decided with the operator: **newest write wins, fully automatic** — when a mirror's
  content no longer matches what the daemon installed, that copy is the newest statement of
  the conversation and becomes the session's source; the "main harness" moves; the same pass
  rewrites every other configured copy from it (including the old source). No quiet window on
  the promoted source's first push: the point is that no harness ever shows a stale copy.
  The daemon ticks every 5 s (`SESYNC_POLL`), so an edit is picked up within ~5 s.
- Implementation: new `sesync/src/turn.rs`. `check` compares a recorded mirror's content
  against its `installed_hash`, cached by a size:mtime `edit_stamp` (one stat per pass when
  untouched; two new `#[serde(default)]` fields `edit_stamp`/`target_updated` on `Mirror`).
  `promote_changed` promotes the newest edit per session and does the state surgery: winner's
  file becomes the source entry, old source's harness becomes a mirror target (its stale copy
  is pre-recorded so `record_mirror` deletes it when the winner's copy lands elsewhere),
  winner removed from `mirror_keys`, loser recorded as a mirror. OpenCode mirror rows are now
  hidden from the scan (`scan_opencode`) and their id stays stable across turns
  (`install_into` keeps the recorded mirror id, so a refresh replaces the same conversation).
  `Stores` gained `opencode_db`; `PassSource` gained `urgent`.
- Regression found live: the promotion **flip-flopped every pass** — the running TUI held a
  dead session in memory and its `save_user` resurrected it on disk (with creation-era
  fingerprints), which turn-taking read as a fresh edit. Fix: `absorb_daemon_state` now
  prunes in-memory sources the daemon removed, keeping runtime-only `tui_created` entries
  (a choice recorded before the first scan).
- Tests: `turn.rs` ×4 (edited mirror wins, touched-with-same-content stays quiet and records
  the stamp, newest of two wins, missing mirror is not a promotion) + `tests/app.rs` prune
  test + `tests/turn_live.rs` ×2 (real daemon binary + real sesync-conv end to end: answer in
  the claude copy promotes it, the omp copy is replaced and holds the answer; two quiet
  passes mint nothing). Suites: `sesync` **496 passed, 0 failed**, `sh build-test.sh` green.
- Deployed (backups `sesync.prev-turn`, `sesync.prev-turn2`), daemon restarted, active; the
  live TestFromOPENCODE session healed itself within one tick (claude copy is the source;
  omp+opencode mirrors refreshed; zero repeated promotions since). The losing omp copy
  (with the omp-side answer, 20:32:33) still exists on disk, hidden as a mirror — it is the
  only place that answer survives; say the word and it can be removed.
- Note: `tui_pty` tests now pin `SESYNC_CONFIG_DIR` into the sandbox — without it a real
  `~/.config/sesync/tui.json` (persisted Team view) opened the test TUI on the Team tab and
  the Sessions-title waits timed out.

## 2026-10-18 (night) — turn-taking hardened after live churn (same branch)

- Operator asked for an efficiency/edge-case review of turn-taking (now the app's most
  crucial part). Review + live testing found the promotion could **churn indefinitely**:
  multiple state entries for one conversation (a promotion leaves the old entry behind on
  disk; the merge re-adopts it) recorded the same copy paths with stale installed-hashes, so
  every pass re-read the daemon's own rewrites as user edits and turned the session again,
  minting a new mirror generation each time (mirror ids were derived from the moving source
  key).
- Fixes, all in shared code (`turn.rs`, `state.rs`, `mirror_write.rs`, `bin/sesync.rs`):
  1. **Stable mirror ids**: `Source::conv` — the conversation's origin uuid/ses id, pinned at
     first scan; `install_into` mints every mirror id from it (falling back to the source
     key for old states). A turn now replaces each store's copy in place; no re-minting, no
     dead files, and the fingerprint/installed-hash bookkeeping stays exact.
  2. **One entry per conversation** (`turn::component_of`): entries that record each other's
     keys as mirror paths are one conversation. A component with a real edit turns to the
     newest edit (ties break by harness name, deterministic); one without collapses to its
     most recently turned entry (`turn_at`). This resolved the live tangle without losing a
     real turn.
  3. **`promote` refuses when the destination copy already holds the entry** — a redundant
     candidate from a leftover can no longer graft stale mirror records onto the live entry.
  4. **Merge hardening**: the daemon does not re-adopt keys hidden in `mirror_keys`
     (`State::adoptable`); the TUI already prunes daemon-removed sources.
  5. Efficiency: `promote_changed` no longer clones the whole sources map per pass (the scan
     is read-only; writes happen after). Cost per tick: one stat per recorded mirror; a file
     read only when the fingerprint moved.
- Tests now cover the crucial loop end to end (`tests/turn_live.rs`, real daemon binary +
  real sesync-conv): turn omp→claude, then a **second real answer in the rewritten omp copy
  turns it back** (the exact loop the churn corrupted), losers replaced not duplicated, and
  two quiet passes mint nothing. Unit tests in `turn.rs` (7): basic turn, quiet touch
  records the stamp, newest-of-two wins, missing mirror is not a promotion, deterministic
  tie-break, legacy `target_updated=0` upgrade never promotes, tangle collapses to one entry
  and stays quiet. `SES_SYNC_TURN_DEBUG` (`SESYNC_TURN_DEBUG`) traces detection decisions.
- Suites: `sesync` **500 passed, 0 failed**, `sh build-test.sh` green. Deployed (backups
  `sesync.prev-turn3`, `sesync.prev-turn4`), daemon active; the live TestFromOPENCODE
  tangle collapsed to its claude winner within one pass and has stayed quiet since.

## 2026-10-18 (night) — PR #43 (`turn-taking` → `session-keys`), CI green (same branch)

- Branch `turn-taking` holds today's work as three commits (converter title/tool-id, mirror
  pipeline + turn-taking, TUI QoL + themes) plus the CI debugging; PR
  https://github.com/y0c0ms/sesync/pull/43 into `session-keys`. All five checks pass:
  unix (ubuntu + macOS), windows, team-server/Postgres, cargo-audit.
- The macOS job failed on `tui_pty::tui_draws_the_session_and_quits_on_q` and was a real
  find: with the full-background themes, one frame is tens of kilobytes (every cell carries
  its own SGR background run); the test stopped reading the pty after the title appeared,
  leaving most of the frame unread in macOS's smaller pty buffer, so the child's next small
  write blocked forever — it never polled again and never saw the `q`. Linux's 64 KB pty
  buffer hid it. Real terminals always drain, so this was test-harness-only; the fix is
  `wait_with_timeout_reading`, which keeps draining the master while waiting for exit
  (all three pty exit waits use it now).
- Debugging aid, kept: `SESYNC_TRACE=<file>` makes the TUI append its startup and event
  trace (start, draw/poll timeout, keys, quit); the pty tests set it into their sandbox and
  print it when a wait fails. That trace is what localized the hang (only "tui start" ever
  logged; no poll timeout ever fired).

- Deployed 19:51: `~/.local/bin/sesync` (backups `sesync.prev-qol`, then 20:0x
  `sesync.prev-bar` for the bar change), daemon restarted, active.
  `sesync-conv` unchanged by this batch. **Restart any running TUI** to get the new keys.
- Still open from the list (server-side, deliberately not started here): removing the legacy
  plaintext `/record` routes (needs the test rework + the commit split, see below) and the
  lockstep-client/first-team-space decision.

## 2026-10-18 (evening) — a rename made inside omp reaches the mirrors (same branch)

- Reported: a session renamed **inside omp** mirrored into Claude fine, but Claude still showed
  the pre-rename name. Three independent gaps:
  1. omp appends a `title_change` record on /rename and overlays it over the 256-byte title
     slot when it reads; `titles::omp_title_from_lines` never read `title_change`, so sesync
     kept the stale slot until omp saved the file again (verified: a 17:01:04 change sat in
     `title_change` while the slot still held the old name).
  2. `journal::title_text` returned the slot before looking at `title_change` — the converter
     lagged the same way.
  3. `to-claude` wrote no title at all (`install::install_into` never passed `--title` for
     Claude), and Claude labels a title-less transcript by its first user message — the "old"
     name the operator saw.
- Fixes: both readers now take the **last `title_change` > slot > last Pi `session_info` >
  header > first user line**; `to-claude [--title T]` (default the journal title) writes
  `{"type":"custom-title","customTitle":…,"sessionId":…}` as the first record — the shape
  Claude's own /rename writes and prefers over `ai-title`; `install_into` passes
  `--id --title` to every target.
- Suites: `sesync` **465 passed, 0 failed** (sandbox; +1 new test
  `omp_title_change_beats_the_initial_slot`), `sesync-conv` **73 passed, 0 failed** (+2:
  rename→custom-title head record, nameless session→no record; the tool-id test below is the
  +1), `sh build-test.sh` green.
- Deployed 18:25 (backups `sesync.prev-title`, `sesync-conv.prev-title`) and healed live: the
  `TESTfromOMP` session (omp source `…tileCoder35b/…_01a12695…`) had its claude mirror record
  cleared, the daemon rewrote it at the same path, and the first record is `custom-title`
  exactly equal to the source's last `title_change`; the opencode mirror already carried the
  current name.
- Same evening, from live testing of the above, two more defects fixed (deployed 18:43,
  backups `sesync.prev-toolid`, `sesync-conv.prev-toolid`):
  1. **TUI rename on an omp session now works inside omp too.** `manage::rewrite_title`'s omp
     arm rewrote only the header title, which omp never overlays onto its slot. It now writes
     the 256-byte title slot as omp's own serializer does (`type/title/v/source:"user"/
     updatedAt/pad`, trimmed to fit) **and** appends a `title_change` entry (8-hex id via
     uuid5, `parentId` = the entry before it, `previousTitle`) — the same two writes omp
     makes on `/rename`. Test fixture is slot-first and asserts slot length 255, source
     `user`, ISO-ms `updatedAt`, and the appended `title_change`.
  2. **omp→claude mirrors lost every tool call.** Claude's session loader drops a `tool_use`
     whose id is not native (`toolu_` + 24 chars); omp mints 32-char tokens and
     `to-claude` passed them through, so Claude rendered only the text (opencode, which is
     laxer, looked fine). `claude.rs` now remaps both call and result deterministically
     (`toolu_` + first 24 hex of uuid5(raw)) at export time; covered by a new
     `a_foreign_tool_id_is_mapped_to_a_native_one` plus pairing asserts in the existing
     round-trip tests.
- Healed the same mirror again after 2: live file now has 118/118 perfectly paired
  `toolu_`-shaped calls/results, `custom-title` first. Operator: reopen the session in Claude
  to re-read the file.

## 2026-10-18 (evening) — legacy mirrors cleared at operator request (same branch)

- Operator: the mirrors the old auto-mirror attempts had created on omp/claude sessions must
  not stay considered mirrors. With the daemon stopped: `sesync mirrors --apply` removed **67**
  file mirrors (unchanged copies; every source still on disk) and kept 6 files whose target
  side has newer content (`--/8b6a5562…` omp + `--/7b598aa6…` pi, claude `7b386d92…`, pi
  `f08dcf40…`, claude `3ef245ef…`, pi `83f04850…`) — remove those in their harnesses if they
  are junk. The 34 OpenCode records are kept by design (remove through OpenCode).
- Still stopped: every `sources.*.mirrors` record was cleared and every `chosen_targets`
  emptied except the `aac4763c…` Test session (operator's live test: omp+opencode), and the 33
  imported OpenCode sessions were deleted via `opencode session delete` (77 → 44 rows; the
  34th id was never in the store). Daemon restarted 15:28; journal quiet; mirror records 0,
  chosen 1. Backups: `/tmp/opencode/state-pre-legacy-cleanup.json`, `state-post-apply.json`.
- Correction to the section below: `sesync mirrors --apply` only plans **recorded** mirrors,
  so superseded `--`-bucket files with no record (4 remain) are not covered by it; they stay
  hidden via `mirror_keys` (111 → 44 keys).
- A TUI that was already running before this cleanup holds the old picks in memory; any
  subsequent keypress would re-save them. Close and reopen the TUI.

## 2026-10-18 (evening) — mirrors land where the harnesses look (same branch)

- Reported: sessions the TUI showed as mirrored never appeared in omp/OpenCode, and a chosen
  stub showed `omp!`. Three independent causes, all on the mirror path:
  1. `mirror_write` passed the target store's parent (`~/.omp`) as the converter's `--home`,
     so a cwd under the real home looked outside-home and omp mirrors landed in
     `--home-yocoms-…-` — a bucket omp never lists.
  2. Older claude→omp/Pi mirrors sat in bucket `--` (written before the cwd was carried into
     the converter); the state still pointed at them, so the daemon saw the session as
     mirrored and never rewrote it.
  3. OpenCode 1.19 moved its store to SQLite (`opencode.db`); all 34 recorded OpenCode mirror
     ids were gone from it.
- Fixes: `buckets` (`seg_join`, `pi_bucket`, `omp_home_bucket`, `cwd_from_head`; no cwd or a
  cwd outside home is never called stale), `scan::mirror_stale` feeding the existing
  `mirror_demand` (wrong bucket, missing file, or an id absent from the store the scan already
  opened), `mirror_write::Stores::home` (the real `Env.home`), and `record_mirror` now removing
  a superseded file only when it is unchanged (`installed_hash` + absolute path).
- Suites: `sesync` **464 passed, 0 failed** (sandbox, then the live team env),
  `sesync-conv` **70 passed, 0 failed**, `sh build-test.sh` green. New tests: buckets (2),
  `mirror_stale` (3), mirror replace/keep (2), omp home-relative bucket through the real
  converter (1).
- Deployed and verified live: the heal re-imported 33 OpenCode sessions (44 → 77 rows in
  `opencode.db`) and rewrote every affected omp/Pi mirror into the right bucket (claude
  `660c446d…` now lives at `~/.omp/agent/sessions/-SkillsOutlie/…` and
  `~/.pi/agent/sessions/--home-yocoms-SkillsOutlie-/…`).
- Live-state note: every `chosen_targets` except the empty stub's had vanished from the live
  state file; the writer could not be identified from the journal, and the current
  `save_user` + `absorb_user_edits` merge cannot reproduce it. The choices were restored from
  the surviving mirror records (38 sources) and the heal above is the result. If choices
  vanish again, copy `~/.local/share/sesync/state.json` before touching anything.
- Open from this (see the cleanup section above): the `aac4763c…` Test stub has no
  user/assistant turn and can never convert — it shows `!` and backs off until it gets one;
  `114272c2…` is no longer chosen. Four superseded files without records remain in the `--`
  buckets, hidden via `mirror_keys`. Commit split still open.

## 2026-10-18 — branch `session-keys`

- HEAD `5e8b66e`. The tree carries uncommitted local-test work on top: TUI state
  persistence, Ctrl+R local refresh, newest-first sessions, and the new tests below. Nothing
  was pushed; do not assume HEAD describes the tree.
- Suites, last run with the live team environment:
  - `sesync`: **464 passed, 0 failed** (456 at the time of this section; 464 after the
    evening mirror-bucket work)
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
    floor raise), and a PTY test drives Enter → session menu → mirror picker → confirm, then
    a real `--once` pass that must write only the chosen harness.
  - **Local correctness (this session)**: the TUI persists its state when a key changes a
    choice (mirror targets, marks — see the lag fix below; before that it saved after every
    key); Ctrl+R on the Sessions tab re-lists in place and
    keeps the cursor (the Team tab still asks for a pull); an explicitly chosen mirror
    bypasses the 14-day age window; the sessions list sorts newest first.
  - **OpenCode as a mirror source**: an OpenCode session can now be mirrored into omp, Pi or
    Claude. The daemon exports it (`opencode export`), converts once (`from-opencode`) and
    installs the same journal into every chosen target; a pending choice also outranks the
    age window, like file sources. Covered end to end with a fake CLI (no harness binary is
    ever run in tests).
  - **OpenCode as a mirror target** (session-migrate's import-bundle approach, strengthened):
    `sesync-conv to-opencode` writes the official bundle and `install::install_opencode` runs
    `opencode import` under the conversation's own directory. Upstream's importer
    conflict-ignores existing message ids, so the writer deletes the deterministic mirror
    session id first (`opencode session delete`) — a refresh replaces the session instead of
    mixing old and new turns. The deterministic id (`mirror::mirror_opencode_id`) and the
    delete-first rule are this repo's improvement over session-migrate's one-shot, new-id
    migration. The TUI offers `opencode` only when the CLI is on PATH; the mirror record
    carries the session id (not a file) and `mirror_cleanup` keeps it (remove through
    OpenCode).
  - **Claude-source conversion fix**: a Claude transcript is now converted
    (`from-claude`) before any target converter sees it. Raw transcripts used to be copied
    through verbatim, so a Claude session chosen into Pi/omp produced a file in the other
    harness's store that was never a journal. Covered by
    `opencode_target::a_claude_source_is_converted_to_a_real_journal_for_the_pi_mirror`.
  - **State ownership**: the daemon and the TUI now share state.json safely. The daemon
    adopts user-owned fields (marks, chosen targets) before every pass and again before
    saving; the TUI saves only those fields over the file the daemon wrote, and absorbs the
    daemon's mirrors/floors about once a second. Before this, a running daemon overwrote a
    TUI choice within one pass — the reported "configured it, nothing happened" bug. A live
    test starts a real `--watch` daemon, saves a choice from outside, and requires the mirror;
    removing the merge makes it fail.
  - **New tests**: harness-side renames are read and TUI renames are written for omp, Pi and
    Claude (manage); Ctrl+R picks up a Claude-side rename (app); an omp rename reaches the
    Pi mirror on the very next pass (cli); the live two-device team test now proves a
    rename reaches the other device at the next sequence and raises its floor; the OpenCode
    target delete/import/refresh order is driven by a fake CLI (`opencode_target`); the real
    CLI path (gated on `HS_E2E=1`) re-imports under the same id and asserts the old turns are
    gone (`opencode_install`).

## 2026-10-18 — Gemini CLI wiring (same branch)

- `session-migrate` has no Gemini CLI adapter (its Google support is Antigravity only); the
  converters were already ours (`from_gemini`/`to_gemini`, validated against Gemini CLI
  0.64.0). This session wired them into the daemon.
- Converter: `to-gemini` resolves the project slug exactly as Gemini does — registry reuse
  (`projects.json`), else the `.project_root` marker, else slugify-with-suffixes — and
  `--project` is now optional (journal header `cwd` fallback). The file name is deterministic
  (`session-<stamp>-<short>.jsonl`), older files for the same mirror id are removed, and the
  write is tmp+rename. New unit tests for `normalize_root`/`slugify`/`resolve_slug`.
- Daemon: Gemini is a source (`discovery::gemini_sessions` in `scan`, `gemini_title_from_lines`)
  and a mirror target (`install::install_gemini`, `Stores.gemini`, the raw-source guard now
  covers gemini, picker probe `GEMINI_CLI_HOME`). The mirror path **is** recorded in
  `mirror_keys` because the Gemini store is also scanned for sources (unlike OpenCode's id).
  Manage lists, titles and rename (`$set.summary`) handle gemini. A Gemini source converted to
  the team copy via `Canonical::Gemini` (`from-gemini`).
- Tests: `gemini_target` (install + registry + marker + mirror_keys + refresh-in-place;
  source→pi round trip), gemini title cases, picker case, raw-source guard case. Two tests that
  used `gemini` as the *unknown* harness/target now use `cursor`/`windsurf` (gemini became
  known); assertions unchanged.
- Suites: `sesync` **448 passed, 0 failed** (live team env), `sesync-conv` **70 passed,
  0 failed**.
- Deployed locally: `~/.local/bin/sesync` and `~/.local/bin/sesync-conv` reinstalled from this
  tree, `sesync.service` restarted (backups: `sesync.prev-gemini`, `sesync-conv.prev-gemini`).
- TUI mirror picker now shows a glyph before each harness name (`ui.rs::harness_icon`):
  ✶ claude · ✦ gemini · Ω omp · ◉ opencode · π pi. The PTY test asserts `[x] π pi` and
  `[x] ✶ claude` on screen. Note: one full-suite run showed a one-off flake in
  `team_sync::an_opencode_session_becomes_a_journal_the_team_can_install` (unreproduced in
  15 retries and two further full runs; the panic message was not captured).

## 2026-10-18 — TUI lag fix (same branch)

- Reported: the TUI felt laggy; Down took time to move to the next session. Cause: the list
  refresh read every session file **whole** (`manage::session_title` →
  `fs::read_to_string`) — Claude transcripts reach hundreds of megabytes — and the TUI ran
  that refresh once a second while idle. Each keypress also loaded, absorbed and re-saved the
  whole state file, so even an idle keystroke paid a JSON round-trip.
- Fixes:
  - `titles::read_title_text`: files over 256 KiB are titled from a 64 KiB head + 64 KiB
    tail window, cut at whole lines (titles live in the first records — headers, ai-title —
    or the last — Pi/omp renames, Claude custom-title, Gemini summary). Used by `manage` and
    by the daemon's `scan`. Smaller files are still read whole.
  - `manage::TitleCache`: titles cached by (size, mtime); a re-list skips unchanged files.
    The TUI keeps one cache for its lifetime (`list_groups_cached`; `list_groups` stays the
    throwaway-cache wrapper for one-off callers).
  - `run_tui` (bin): the state file is stat'ed on each 200 ms tick; `State::load` + absorb +
    `refresh_rows` run only when (mtime, size) changed, so an idle second costs one stat.
    State is saved only after a key that changed a user field (`App::take_user_dirty`),
    not after every keypress.
- Tests: `big_transcripts_are_titled_from_their_head_or_tail` (files past the whole-read
  limit, title at head and at tail), `the_title_cache_re_reads_a_file_only_when_it_changed`,
  `only_a_key_that_changes_a_choice_marks_state_for_saving`.
- Suites: `sesync` **451 passed, 0 failed** (sandbox and live team env), `sh build-test.sh`
  all passed. Deployed: `~/.local/bin/sesync` reinstalled (backup `sesync.prev-lag`),
  `sesync.service` restarted. A running TUI must be restarted to pick up the new binary.

## 2026-10-18 — mirror failures visible and bounded (same branch)

- Reported: a Claude session mirrored into omp never appeared, and the TUI showed nothing.
  The daemon log showed the real path: the chosen transcript is a 6-line **stub** (system /
  mode / permission-mode / last-prompt / custom-title, no user or assistant turn) that the
  TUI listed like any session; `sesync-conv from-claude` correctly refuses it, the write
  failed, and the daemon retried every 5 s forever (`failures` went up, but nothing used it).
  The TUI had no way to show a chosen-but-unwritten target, so the failure was invisible.
- Fixes:
  - `from_claude` now skips and counts unparseable lines (`stats.skipped_bad_lines`, reported
    on stderr) instead of failing the whole conversion — matching `from_gemini`. A truncated
    line in a real transcript must not lose the session. (Survey note: the operator's store
    currently has **no** unparseable lines; the 6-line stub has simply no turns and still
    refuses, which is correct.)
  - Retry backoff: `scan::mirror_retry_due` + `scan::mirror_demand`. A chosen target with no
    mirror forces a pass, but after failures the wait is the quiet period, doubling to 10 min
    (`Source::last_failure`, new state field, additive). A content change (`hash_changed`)
    still retries at once; a fully successful pass clears `failures`/`last_failure`
    (`pass::run_pass` and `bin/sesync.rs::sync_chosen` both record and clear).
  - MIRRORS column: a chosen target with no file shows `pi?` (pending) or `pi!` (last write
    failing) (`app::session_cols`), so a broken mirror can be seen and unticked.
- Tests: `from_claude::unparseable_lines_are_skipped_and_counted`, the retry/demand unit tests
  in `scan` (`a_backing_off_missing_mirror_does_not_keep_the_source_due`,
  `a_mirror_for_an_unchosen_target_does_not_satisfy_a_missing_choice`), `state` round trip of
  `last_failure`, and `session_menu::a_chosen_mirror_with_no_file_yet_is_marked_pending_or_failed`.
- Suites: `sesync` **456 passed, 0 failed** (sandbox and live team env), `sesync-conv`
  **70 passed, 0 failed**, `sh build-test.sh` all passed. Deployed: `sesync` and
  `sesync-conv` reinstalled, `sesync.service` restarted; verified live that the failing stub
  is attempted once and then backs off (1 attempt in the first minute, next due after 10 min).
- Note for the operator: the stub's omp choice is still recorded (it will retry every
  10 min and show `omp!`). Open its options and untick omp, or pick the real session from
  the list; a running TUI must be restarted to load this build.

## Deployment

- Homelab `team-server` (namespace `team`, ArgoCD app `team-server`) now runs image
  `192.168.100.178:30950/yocoms/team-server:5e8b66e` (digest `a3d0da5…`), rolled out via
  hlgit commits `edefa92` + `fe37290` (admin token SealedSecret). `/healthz` is 200, the new
  tables exist, the admin routes answer; the DB is empty (no space yet).
- `~/.local/bin/sesync` and `sesync-conv` are reinstalled from the current tree;
  `sesync.service` is active (latest reinstall for the full-background themes, backup
  `sesync.prev-themes`; earlier: 20:0x `sesync.prev-bar`, 19:51 `sesync.prev-qol`,
  18:43 `sesync.prev-toolid` / `sesync-conv.prev-toolid`, 18:25 `sesync.prev-title`,
  14:55 `sesync.prev-buckets`).
  The admin token lives at `~/.config/sesync/admin.token` (0600); first space
  creation needs the admin key and `SESYNC_ADMIN_TOKEN_FILE`.

## Blocked / waiting

- Commit split: the uncommitted work needs an operator-approved set of commits before a
  deploy tag can point at a real revision.
- Plaintext `/record` and `/record/read` routes still exist for legacy tests
  (`http_flow`, `large_session`, `socket`, `push_e2e`); clients no longer use them.
- The live team suite is not in CI (needs Postgres + a built `team-server`).
- `team-backup` timer still needs approval; `sesync.service` disable decision is open.
- Share/retract UI actions are still disabled (no server path yet).

## Structural debt (next model, read this)

- `sesync/src/bin/sesync.rs` (~960 lines) mixes argument dispatch, environment loading, the
  daemon loop, the TUI launcher and the team CLI. Split it into focused modules when it is
  next touched; do not grow it.
- `sesync/src/app.rs` (~1670 lines) holds every TUI mode and handler. New modes should go in
  their own module once they need more than a screen of code; the QoL batch grew it further.
- `team_pull`, `team_conflict` and `divergence` are test-only; delete them or revive them
  deliberately, but do not add callers without a decision.
- The plaintext `/record` routes exist only for old tests; remove them together with the test
  rework, not piecemeal.

## Next action

1. Operator: relaunch any running TUI so it picks up the 2026-10-21 binaries.
2. Operator decisions still open: how revocation stays revoked (rotate the authenticator
   secret on removal, a remove-member route, or a `publish` that lists the devices it signs);
   the commit/PR split of the 2026-10-21 work; `sesync.service` enablement; plaintext route
   removal; lockstep client update on the other machines.
3. Optional follow-up: an agy lock hook (agy turns are excluded only by the converter's
   in-use refusal and the archive), and reading Antigravity App conversations
   (`~/.gemini/antigravity/conversations/*.db`, same format) as sources.
