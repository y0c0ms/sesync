//! The team lifecycle end to end: create, the one-time authenticator URI and its QR, join, the
//! admin's approval, renew, revoke, push and pull, and the daemon's team step. A real team server
//! runs in-process over a real socket (support/team_server.rs); every call goes through the client
//! code the CLI, the TUI and the daemon use, or through the sesync binary itself in a sandbox.
#[path = "support/team_server.rs"]
mod support;

use qrcodegen::QrCode;
use ratatui::{backend::TestBackend, buffer::Buffer, style::Color, Terminal};
use sesync::device_keys::load_or_create;
use sesync::sealed::seal_for_devices;
use sesync::state::State;
use sesync::team_cli::Command;
use sesync::team_join::{self, credential_path, trust_of, Approval};
use sesync::team_push::{push_marked_sealed, HttpTeam};
use sesync::team_watch::{all_teams, fetch_and_verify, teams_from_env, TeamWatch};
use sesync_team::totp::{code_at, LOCK_SECS};
use std::collections::BTreeMap;
use std::path::Path;
use support::{sandbox, sesync_cmd, wall_now, TeamServer, ADMIN_TOKEN, DAY};

/// A fixed server time, mid-step, so a code never straddles a 30-second boundary in a test.
const T0: u64 = 1_000_020;

// ----- helpers ---------------------------------------------------------------------------------

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).expect("the label decodes to UTF-8")
}

/// Read an otpauth URI the way an authenticator app does: the fragment is cut first, the query
/// next, and the label is the single path segment after `totp/`, percent-decoded.
fn parse_otpauth(uri: &str) -> (String, BTreeMap<String, String>) {
    let no_fragment = uri.split('#').next().unwrap();
    let rest = no_fragment.strip_prefix("otpauth://totp/").unwrap_or_else(|| panic!("not a TOTP URI: {uri}"));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    assert!(!path.contains('/'), "the label is one path segment: {uri}");
    let params = query.split('&').filter(|p| !p.is_empty())
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect();
    (percent_decode(path), params)
}

fn base32_decode(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut buf, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for c in s.trim_end_matches('=').bytes() {
        let v = ALPHABET.iter().position(|a| *a == c.to_ascii_uppercase()).expect("base32") as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// A writer pushes `text` as `conversation` through the push path the daemon uses: the session is
/// shared with the team, the lease version becomes the record sequence, the record is sealed for
/// the current list.
fn push(srv: &TeamServer, writer_cfg: &Path, space: &str, conversation: &str, text: &[u8]) -> Result<(), String> {
    let cred = team_join::load(&credential_path(writer_cfg, space))?;
    let pinned = trust_of(&cred).ok_or("no trust root pinned")?;
    let trust = fetch_and_verify(&srv.base, &cred.token, &cred.machine, space, &pinned).unwrap_or(pinned);
    let writer = load_or_create(writer_cfg)?;
    let mut state = State::default();
    state.share(conversation, space);
    let remote = HttpTeam { base: srv.base.clone(), token: cred.token.clone(), machine: cred.machine.clone() };
    push_marked_sealed(&state, &remote, space, conversation, |version| {
        seal_for_devices(&writer, &trust, space, conversation, version, text).map_err(|e| format!("{e:?}"))
    })
    .map_err(|e| format!("{e:?}"))
}

/// A reader's pull, exactly as the daemon builds it from the stored credential.
fn pull(srv: &TeamServer, reader_cfg: &Path, space: &str) -> sesync::team_pull_verified::PullOutcome {
    let cred = team_join::load(&credential_path(reader_cfg, space)).unwrap();
    let watch = TeamWatch {
        base: srv.base.clone(),
        token: cred.token.clone(),
        machine: cred.machine.clone(),
        space: space.to_string(),
        device: load_or_create(reader_cfg).unwrap(),
        trust: trust_of(&cred),
    };
    watch.pull_all(&BTreeMap::new())
}

fn other_code(good: &str) -> String {
    if good == "123456" { "654321".into() } else { "123456".into() }
}

// ----- the authenticator URI and its QR --------------------------------------------------------

#[test]
fn the_authenticator_uri_carries_any_team_name_and_the_code_from_it_joins() {
    let srv = TeamServer::start(T0);
    for name in ["Ops Team", "R&D #1", "équipe", "sync=on"] {
        let admin = sandbox("uri-admin");
        let uri = srv.create(&admin, name, &["alice"]);
        assert!(uri.is_ascii() && !uri.contains(' '), "a URI has no raw space or non-ASCII byte: {uri}");
        let (label, params) = parse_otpauth(&uri);
        assert_eq!(label, format!("sesync:{name}"), "the app shows the team's own name: {uri}");
        assert_eq!(params.get("issuer").map(String::as_str), Some("sesync"), "{uri}");
        let secret = base32_decode(params.get("secret").unwrap_or_else(|| panic!("no secret in {uri}")));
        assert_eq!(secret, srv.secret(name), "the URI carries the team's secret: {uri}");
        // The phone computes the code from what it scanned, and that code joins.
        let code = code_at(&secret, srv.now());
        let cfg = sandbox("uri-member");
        team_join::join(&srv.base, name, "alice", "phone-box", &code, &cfg)
            .unwrap_or_else(|e| panic!("the code from the scanned URI joins '{name}': {e}"));
    }
}

fn qr_app() -> sesync::app::App {
    let d = sandbox("qr-app");
    for sub in ["omp/sessions", "pi", "claude"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    sesync::app::App::new(
        sesync::manage::Roots::under(&d, d.join("no-sesync-conv")),
        State::default(),
        d.join("locks"),
        d.join("no-sesync_lock"),
    )
}

fn render(app: &sesync::app::App, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| sesync::ui::draw(f, app)).unwrap();
    term.backend().buffer().clone()
}

fn text_of(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Read the drawn code back from the buffer, module by module, and check it is exactly `code`
/// with a blank quiet zone of `quiet` modules: one column per module and two modules per row, so
/// it is square on a terminal whose cells are twice as tall as they are wide.
fn assert_drawn_code(buf: &Buffer, code: &QrCode, quiet: i32) {
    // The code is the block of cells painted on the white QR background.
    let white = Color::Rgb(0xFF, 0xFF, 0xFF);
    let cells: Vec<(u16, u16)> = (0..buf.area.height)
        .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
        .filter(|&(x, y)| buf[(x, y)].bg == white)
        .collect();
    assert!(!cells.is_empty(), "a QR is drawn:\n{}", text_of(buf));
    let x0 = cells.iter().map(|c| c.0).min().unwrap();
    let x1 = cells.iter().map(|c| c.0).max().unwrap();
    let y0 = cells.iter().map(|c| c.1).min().unwrap();
    let y1 = cells.iter().map(|c| c.1).max().unwrap();
    let n = code.size() + 2 * quiet;
    assert_eq!((x1 - x0 + 1) as i32, n, "one column per module, quiet zone included:\n{}", text_of(buf));
    assert_eq!((y1 - y0 + 1) as i32, (n + 1) / 2, "two modules per row:\n{}", text_of(buf));
    for py in 0..n {
        for px in 0..n {
            let cell = &buf[(x0 + px as u16, y0 + (py / 2) as u16)];
            let (top, bottom) = match cell.symbol() {
                "█" => (true, true),
                "▀" => (true, false),
                "▄" => (false, true),
                " " => (false, false),
                other => panic!("unexpected glyph {other:?} in the QR"),
            };
            let drawn = if py % 2 == 0 { top } else { bottom };
            let (mx, my) = (px - quiet, py - quiet);
            let want = mx >= 0 && my >= 0 && mx < code.size() && my < code.size() && code.get_module(mx, my);
            assert_eq!(drawn, want, "module ({mx}, {my}) as drawn");
        }
    }
}

#[test]
fn the_qr_for_a_real_team_encodes_the_same_account_and_is_drawn_whole_and_square() {
    let srv = TeamServer::start(T0);
    let uri = srv.create(&sandbox("qr-admin"), "personal", &["alice"]);
    // The screen encodes the URI without the parameters that restate the defaults; an
    // authenticator reads it as the same account, secret and issuer.
    let compact = sesync::qr::compact_uri(&uri);
    let (label, params) = parse_otpauth(&uri);
    let (c_label, c_params) = parse_otpauth(&compact);
    assert_eq!(c_label, label);
    assert_eq!(c_params["secret"], params["secret"]);
    assert_eq!(c_params["issuer"], params["issuer"]);
    for (k, default) in [("algorithm", "SHA1"), ("digits", "6"), ("period", "30")] {
        assert!(params.get(k).map_or(true, |v| v == default), "the server's {k} is the default one");
        assert!(!c_params.contains_key(k), "{k} is left to the authenticator's default");
    }
    let code = QrCode::encode_text(&compact, sesync::qr::ECC).unwrap();
    let mut app = qr_app();
    app.show_qr("personal", &uri);
    let buf = render(&app, 80, 40);
    assert_drawn_code(&buf, &code, sesync::qr::QUIET);
    assert!(text_of(&buf).contains(&sesync::qr::grouped(&params["secret"])),
        "the setup key is shown for manual entry too:\n{}", text_of(&buf));
}

#[test]
fn an_80x24_terminal_holds_the_whole_code_with_the_narrow_quiet_zone() {
    let srv = TeamServer::start(T0);
    let uri = srv.create(&sandbox("qr-80x24-admin"), "personal", &["alice"]);
    let code = QrCode::encode_text(&sesync::qr::compact_uri(&uri), sesync::qr::ECC).unwrap();
    let mut app = qr_app();
    app.show_qr("personal", &uri);
    let buf = render(&app, 80, 24);
    assert_drawn_code(&buf, &code, sesync::qr::QUIET_MIN);
    let (_, params) = parse_otpauth(&uri);
    assert!(text_of(&buf).contains(&sesync::qr::grouped(&params["secret"])), "{}", text_of(&buf));
}

#[test]
fn a_terminal_too_small_for_the_qr_shows_the_setup_key_instead_of_a_clipped_code() {
    let srv = TeamServer::start(T0);
    let uri = srv.create(&sandbox("qr-small-admin"), "personal", &["alice"]);
    let mut app = qr_app();
    app.show_qr("personal", &uri);
    let text = text_of(&render(&app, 60, 18));
    assert!(!text.contains('█') && !text.contains('▀') && !text.contains('▄'),
        "a clipped QR does not scan, so none is drawn:\n{text}");
    assert!(text.contains("the code needs"), "it says how large the terminal must be:\n{text}");
    let (_, params) = parse_otpauth(&uri);
    assert!(text.contains(&sesync::qr::grouped(&params["secret"])), "the one-time key is still shown, as text:\n{text}");
}

// ----- create, invite and join -----------------------------------------------------------------

#[test]
fn a_refused_team_create_leaves_the_name_free() {
    let srv = TeamServer::start(T0);
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let r = agent.post(&format!("{}/admin/team", srv.base))
        .set("Authorization", &format!("Bearer {ADMIN_TOKEN}"))
        .send_json(serde_json::json!({
            "space": "personal", "members": ["alice"],
            "admin_public": "00".repeat(32), "devices": [], "signed_list": "not hex",
        }));
    match r {
        Err(ureq::Error::Status(400, _)) => {}
        other => panic!("a malformed trust root is refused with 400, got {:?}", other.map(|r| r.status())),
    }
    // Nothing of the refused attempt is left: the same name can be created properly at once.
    srv.create(&sandbox("refused-admin"), "personal", &["alice"]);
}

#[test]
fn names_a_token_cannot_carry_are_refused_before_anyone_joins() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("pipe-admin");
    let cmd = sesync::team_cli::Command::Create { name: "pipes".into(), members: vec!["ops|lead".into()] };
    let err = sesync::team_cli::run_detailed(&cmd, &srv.base, ADMIN_TOKEN, &admin).unwrap_err();
    assert!(err.contains('|'), "the refusal names the character: {err}");

    srv.create(&admin, "personal", &["alice"]);
    let invite = sesync::team_cli::Command::Invite { name: "personal".into(), member: "a|b".into() };
    let err = sesync::team_cli::run_detailed(&invite, &srv.base, ADMIN_TOKEN, &admin).unwrap_err();
    assert!(err.contains('|'), "{err}");

    // A machine name with the separator would get a token that never authenticates.
    let cfg = sandbox("pipe-member");
    let err = team_join::join(&srv.base, "personal", "alice", "box|1", &srv.code("personal"), &cfg).unwrap_err();
    assert!(err.contains('|'), "{err}");
    assert!(!credential_path(&cfg, "personal").exists(), "no credential is left for a refused join");
}

#[test]
fn every_team_name_gets_its_own_credential_inside_the_teams_directory() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("files-admin");
    let cfg = sandbox("files-member");
    let names = ["ops/infra", "a:b", "why?", "../escape", "back\\slash", "plain", "100%"];
    for name in names {
        srv.create(&admin, name, &["alice"]);
        srv.join(&cfg, name, "alice", "box-1");
    }
    let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(all_teams(&cfg), want, "every joined team is found again, under its own name");
    for name in names {
        let p = credential_path(&cfg, name);
        assert_eq!(p.parent(), Some(cfg.join("teams").as_path()), "directly in teams/: {p:?}");
        assert_eq!(team_join::load(&p).unwrap().space, name);
    }
    assert!(!cfg.join("escape.cred").exists(), "nothing is written outside teams/");
}

#[test]
fn wrong_codes_lock_the_member_whatever_machine_name_they_come_from() {
    let srv = TeamServer::start(T0);
    srv.create(&sandbox("lock-admin"), "personal", &["alice", "bob"]);
    let cfg = sandbox("lock-member");
    let good = srv.code("personal");
    let wrong = other_code(&good);
    for i in 0..5 {
        let e = team_join::join(&srv.base, "personal", "alice", &format!("guess-{i}"), &wrong, &cfg).unwrap_err();
        assert!(e.contains("401"), "a wrong code is refused: {e}");
    }
    // A fresh machine name does not buy fresh guesses: the member is locked, and told so.
    let e = team_join::join(&srv.base, "personal", "alice", "guess-5", &good, &cfg).unwrap_err();
    assert!(e.contains("locked"), "the lock is named, so the user waits instead of retrying: {e}");
    // Another member is not locked out by alice's failures.
    team_join::join(&srv.base, "personal", "bob", "bob-box", &good, &sandbox("lock-bob")).expect("bob still joins");
    // The lock ends.
    srv.set_now(T0 + LOCK_SECS + 1);
    team_join::join(&srv.base, "personal", "alice", "alice-box", &srv.code("personal"), &cfg)
        .expect("alice joins once the lock has passed");
}

#[test]
fn a_code_already_used_on_this_machine_says_so() {
    let srv = TeamServer::start(T0);
    srv.create(&sandbox("reuse-admin"), "personal", &["alice"]);
    let cfg = sandbox("reuse-member");
    let code = srv.code("personal");
    team_join::join(&srv.base, "personal", "alice", "box-1", &code, &cfg).unwrap();
    let e = team_join::join(&srv.base, "personal", "alice", "box-1", &code, &cfg).unwrap_err();
    assert!(e.contains("already used"), "the user is told to wait for the next code: {e}");
}

#[test]
fn a_code_typed_as_the_authenticator_shows_it_joins() {
    // Authenticator apps show the code in two groups, "123 456"; that is what people type.
    let srv = TeamServer::start(T0);
    srv.create(&sandbox("spaced-admin"), "personal", &["alice"]);
    let code = srv.code("personal");
    let shown = format!("{} {}", &code[..3], &code[3..]);
    team_join::join(&srv.base, "personal", "alice", "box-1", &shown, &sandbox("spaced-member"))
        .expect("the grouped code joins");
    let e = team_join::join(&srv.base, "personal", "alice", "box-2", "12345", &sandbox("short")).unwrap_err();
    assert!(e.contains("6 digits"), "a short code is still refused locally: {e}");
}

#[test]
fn revoking_a_machine_name_that_is_not_listed_is_an_error_not_a_revocation() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("typo-admin");
    let cfg = sandbox("typo-member");
    srv.create(&admin, "personal", &["alice"]);
    srv.admit(&admin, &cfg, "personal", "alice", "laptop");
    // A typo must not read as "it is out at once" while the real device stays in.
    let typo = sesync::team_cli::Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "laptpo".into() };
    let e = sesync::team_cli::run_detailed(&typo, &srv.base, ADMIN_TOKEN, &admin).unwrap_err();
    assert!(e.contains("laptpo") && e.contains("nothing was signed"), "the refusal names the machine: {e}");
    let c = team_join::load(&credential_path(&cfg, "personal")).unwrap();
    assert!(fetch_and_verify(&srv.base, &c.token, &c.machine, "personal", &trust_of(&c).unwrap()).is_some(),
        "the real device is untouched");
    // The real name still revokes.
    let real = sesync::team_cli::Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "laptop".into() };
    sesync::team_cli::run_detailed(&real, &srv.base, ADMIN_TOKEN, &admin).unwrap();
    assert!(fetch_and_verify(&srv.base, &c.token, &c.machine, "personal", &trust_of(&c).unwrap()).is_none(),
        "a revoked device is out at once");
}

// ----- renew -----------------------------------------------------------------------------------

#[test]
fn rejoining_with_the_code_after_the_window_restarts_silent_renewal() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("window-admin");
    let cfg = sandbox("window-member");
    srv.create(&admin, "personal", &["alice"]);
    srv.admit(&admin, &cfg, "personal", "alice", "box-1");

    srv.set_now(T0 + 8 * DAY);
    let e = team_join::renew(None, &cfg, "personal").unwrap_err();
    assert!(e.contains("401"), "past the 7-day window the code is needed: {e}");
    srv.join(&cfg, "personal", "alice", "box-1");

    // Two days after the new code: its 24 h token has expired, its new window has not.
    srv.set_now(T0 + 10 * DAY);
    team_join::renew(None, &cfg, "personal").expect("silent renewal works again after the code");
    let c = team_join::load(&credential_path(&cfg, "personal")).unwrap();
    assert!(fetch_and_verify(&srv.base, &c.token, &c.machine, "personal", &trust_of(&c).unwrap()).is_some(),
        "the renewed token works for a member call");
}

#[test]
fn a_team_joined_with_a_typed_server_address_works_without_sesync_team_url() {
    let srv = TeamServer::start(wall_now());
    let admin = sandbox("typed-admin");
    let cfg = sandbox("typed-member");
    srv.create(&admin, "personal", &["alice"]);
    // The TUI's join dialog: the user types the address, here with a trailing slash.
    let joined = team_join::join(&format!("{}/", srv.base), "personal", "alice", "box-1", &srv.code("personal"), &cfg).unwrap();
    assert!(joined.pending, "a new key waits for the admin");
    srv.approve(&admin, "personal", "alice", "box-1", Some(&joined.fingerprint));
    team_join::check_approval(None, &cfg, "personal").unwrap();

    // The daemon and the TUI find the team, at the server it was joined on.
    let teams = teams_from_env(&cfg);
    assert_eq!(teams.iter().map(|t| (t.space.as_str(), t.base.trim_end_matches('/'))).collect::<Vec<_>>(),
               vec![("personal", srv.base.as_str())]);

    // `sesync team renew`, with no SESYNC_TEAM_URL at all.
    let sb = sandbox("typed-bin");
    let out = sesync_cmd(&sb, &cfg).args(["team", "renew", "personal"]).output().unwrap();
    assert!(out.status.success(), "renew uses the server the team was joined on: {}",
        String::from_utf8_lossy(&out.stderr));
}

// ----- revocation, push and pull ---------------------------------------------------------------

#[test]
fn a_forgotten_device_can_neither_push_nor_pull_and_its_records_stop_showing() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("revoke-admin");
    let wcfg = sandbox("revoke-writer");
    let rcfg = sandbox("revoke-reader");
    srv.create(&admin, "personal", &["writer", "reader"]);
    srv.admit(&admin, &wcfg, "personal", "writer", "writer-box");
    srv.admit(&admin, &rcfg, "personal", "reader", "reader-box");
    push(&srv, &wcfg, "personal", "ses_1", b"before the revocation").unwrap();
    assert_eq!(pull(&srv, &rcfg, "personal").shown.len(), 1);

    let revoke = sesync::team_cli::Command::Revoke { name: "personal".into(), member: "writer".into(), machine: "writer-box".into() };
    sesync::team_cli::run_detailed(&revoke, &srv.base, ADMIN_TOKEN, &admin).unwrap();

    assert!(push(&srv, &wcfg, "personal", "ses_2", b"after the revocation").is_err(), "a revoked device cannot push");
    let mine = pull(&srv, &wcfg, "personal");
    assert!(mine.shown.is_empty(), "a revoked device reads nothing");
    let theirs = pull(&srv, &rcfg, "personal");
    assert!(theirs.shown.is_empty(), "a revoked author's record is not shown");
    assert_eq!(theirs.dropped, 1, "it is counted as dropped");
}

#[test]
fn a_team_named_with_query_characters_pushes_and_pulls() {
    let srv = TeamServer::start(T0);
    for name in ["R&D", "C++", "100% sync", "#ops", "a=b"] {
        let admin = sandbox("q-admin");
        let wcfg = sandbox("q-writer");
        let rcfg = sandbox("q-reader");
        srv.create(&admin, name, &["writer", "reader"]);
        srv.admit(&admin, &wcfg, name, "writer", "writer-box");
        srv.admit(&admin, &rcfg, name, "reader", "reader-box");
        push(&srv, &wcfg, name, "ses_q", b"shared text").unwrap_or_else(|e| panic!("push into '{name}': {e}"));
        let out = pull(&srv, &rcfg, name);
        assert_eq!(out.shown.len(), 1, "the reader of '{name}' gets the session");
        assert_eq!(out.shown[0].text, b"shared text");
    }
}

// ----- the daemon's team step ------------------------------------------------------------------

#[test]
fn the_daemon_renews_an_expired_token_inside_the_window_and_keeps_pulling() {
    let real = wall_now();
    let srv = TeamServer::start(real - 2 * DAY);
    let admin = sandbox("auto-admin");
    let wcfg = sandbox("auto-writer");
    let rcfg = sandbox("auto-reader");
    srv.create(&admin, "personal", &["writer", "reader"]);
    srv.admit(&admin, &wcfg, "personal", "writer", "writer-box");
    srv.admit(&admin, &rcfg, "personal", "reader", "reader-box");
    push(&srv, &wcfg, "personal", "ses_r", b"two days old").unwrap();
    // Two days on: every 24 h token has expired, the 7-day renewal window has not.
    srv.set_now(real);
    let before = team_join::load(&credential_path(&rcfg, "personal")).unwrap().token;

    let sb = sandbox("auto-bin");
    let out = sesync_cmd(&sb, &rcfg)
        .arg("--watch")
        .env("SESYNC_TEAM_URL", &srv.base)
        .env("SESYNC_PASSES", "1")
        .env("SESYNC_POLL", "0")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("team pull [personal]: 1 shown"), "the daemon renews and pulls: {stderr}");
    let after = team_join::load(&credential_path(&rcfg, "personal")).unwrap().token;
    assert_ne!(after, before, "the renewed token is stored for the next run");
}

/// One reader, joined to `spaces`, with `records` sessions pushed into each by a writer.
fn reader_with_teams(srv: &TeamServer, spaces: &[(&str, usize)]) -> std::path::PathBuf {
    let admin = sandbox("backoff-admin");
    let wcfg = sandbox("backoff-writer");
    let rcfg = sandbox("backoff-reader");
    for (space, records) in spaces {
        srv.create(&admin, space, &["writer", "reader"]);
        srv.admit(&admin, &wcfg, space, "writer", "writer-box");
        srv.admit(&admin, &rcfg, space, "reader", "reader-box");
        for i in 0..*records {
            push(srv, &wcfg, space, &format!("ses_{i}"), b"unchanged").unwrap();
        }
    }
    rcfg
}

fn watch_passes(rcfg: &Path, base: &str, passes: u32) -> String {
    let sb = sandbox("backoff-bin");
    let out = sesync_cmd(&sb, rcfg)
        .arg("--watch")
        .env("SESYNC_TEAM_URL", base)
        .env("SESYNC_PASSES", passes.to_string())
        .env("SESYNC_POLL", "0.05")
        .env("SESYNC_TEAM_POLL_MAX", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    stderr
}

#[test]
fn the_team_interval_backs_off_while_a_team_with_records_has_nothing_new() {
    let srv = TeamServer::start(wall_now());
    let rcfg = reader_with_teams(&srv, &[("personal", 1)]);
    let stderr = watch_passes(&rcfg, &srv.base, 4);
    assert!(stderr.contains("team pull [personal]: 1 shown"), "{stderr}");
    assert!(stderr.contains("team poll interval 0.1s"),
        "re-reading an unchanged record is not activity, so the interval doubles: {stderr}");
}

#[test]
fn two_teams_of_different_sizes_back_off_together() {
    let srv = TeamServer::start(wall_now());
    let rcfg = reader_with_teams(&srv, &[("alpha", 1), ("beta", 0)]);
    let stderr = watch_passes(&rcfg, &srv.base, 4);
    assert!(stderr.contains("team pull [alpha]: 1 shown") && stderr.contains("team pull [beta]: 0 shown"), "{stderr}");
    assert!(stderr.contains("team poll interval 0.1s"),
        "one team's size is not compared with another's, so the interval doubles: {stderr}");
}

// ----- membership: only the admin admits, and a revocation sticks -------------------------------

/// The status a member route gives this credential's token, as the daemon would call it.
fn member_status(srv: &TeamServer, cfg: &Path, space: &str, route: &str) -> u16 {
    let c = team_join::load(&credential_path(cfg, space)).unwrap();
    let mut body = serde_json::json!({"space": space, "machine": c.machine});
    if route.ends_with("read") {
        body["conversation"] = serde_json::json!("ses_1");
    }
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    match agent.post(&format!("{}{route}", srv.base)).set("Authorization", &format!("Bearer {}", c.token)).send_json(body) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{route}: {e}"),
    }
}

/// The list version the server holds, as the admin reads it.
fn list_version(srv: &TeamServer, space: &str) -> u64 {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let r: serde_json::Value = agent.post(&format!("{}/admin/roster", srv.base))
        .set("Authorization", &format!("Bearer {ADMIN_TOKEN}"))
        .send_json(serde_json::json!({"space": space})).unwrap().into_json().unwrap();
    r["version"].as_u64().unwrap()
}

fn run_admin(srv: &TeamServer, admin: &Path, cmd: Command) -> Result<String, String> {
    sesync::team_cli::run(&cmd, &srv.base, ADMIN_TOKEN, admin)
}

#[test]
fn a_pending_device_gets_no_list_and_no_data_until_the_admin_approves_it_then_syncs() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("pend-admin");
    let wcfg = sandbox("pend-writer");
    let ncfg = sandbox("pend-new");
    srv.create(&admin, "personal", &["writer", "newbie"]);
    srv.admit(&admin, &wcfg, "personal", "writer", "writer-box");
    push(&srv, &wcfg, "personal", "ses_1", b"before the newcomer").unwrap();

    let joined = team_join::join(&srv.base, "personal", "newbie", "new-box", &srv.code("personal"), &ncfg).unwrap();
    assert!(joined.pending, "a new key waits for the admin");
    assert_eq!(joined.fingerprint, team_join::own_fingerprint(&ncfg).unwrap());
    let c = team_join::load(&joined.path).unwrap();
    assert!(c.admin_public.is_some() && c.signed_list.is_none() && c.list_version.is_none(),
        "the join reply carried the admin key and no list");
    for route in ["/list", "/sessions", "/record/sealed/read"] {
        assert_eq!(member_status(&srv, &ncfg, "personal", route), 403, "{route} refuses a pending device");
    }
    assert_eq!(team_join::check_approval(None, &ncfg, "personal").unwrap(),
        Some(Approval::Waiting { machine: "new-box".into(), fingerprint: joined.fingerprint.clone() }));
    assert!(teams_from_env(&ncfg).is_empty(), "a waiting join has nothing to pull");

    // The admin sees the request with the same fingerprint, and approves it (typed in capitals).
    let roster = run_admin(&srv, &admin, Command::Roster { name: "personal".into() }).unwrap();
    assert!(roster.contains("newbie / new-box") && roster.contains(&joined.fingerprint), "{roster}");
    let text = srv.approve(&admin, "personal", "newbie", "new-box", Some(&joined.fingerprint.to_uppercase()));
    assert!(text.contains(&joined.fingerprint), "{text}");
    assert!(matches!(team_join::check_approval(None, &ncfg, "personal").unwrap(), Some(Approval::Approved { .. })));
    for route in ["/list", "/sessions"] {
        assert_eq!(member_status(&srv, &ncfg, "personal", route), 200, "{route} serves an approved device");
    }
    // And it syncs: a record sealed after the approval reaches it.
    push(&srv, &wcfg, "personal", "ses_2", b"after the approval").unwrap();
    let out = pull(&srv, &ncfg, "personal");
    assert!(out.shown.iter().any(|r| r.conversation == "ses_2" && r.text == b"after the approval"), "{out:?}");
}

#[test]
fn a_revoked_device_is_refused_at_once_and_only_a_new_approval_lets_anyone_back() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("rv-admin");
    let cfg = sandbox("rv-member");
    let other = sandbox("rv-other");
    srv.create(&admin, "personal", &["alice", "bob"]);
    srv.admit(&admin, &cfg, "personal", "alice", "laptop");
    assert_eq!(member_status(&srv, &cfg, "personal", "/sessions"), 200);

    let text = run_admin(&srv, &admin, Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "laptop".into() }).unwrap();
    assert!(text.contains("revoked"), "{text}");
    for route in ["/list", "/sessions", "/record/sealed/read"] {
        assert_eq!(member_status(&srv, &cfg, "personal", route), 403, "{route}: the old token is refused at once");
    }

    // The same key at the door, with a good code: refused, and no credential changes.
    srv.set_now(T0 + 60);
    let e = team_join::join(&srv.base, "personal", "alice", "laptop", &srv.code("personal"), &cfg).unwrap_err();
    assert!(e.contains("403") && e.contains("removed from the team"), "{e}");

    // A new key from the same person only asks; another approval does not sign it.
    srv.set_now(T0 + 120);
    let fresh = sandbox("rv-fresh");
    let j = team_join::join(&srv.base, "personal", "alice", "laptop", &srv.code("personal"), &fresh).unwrap();
    assert!(j.pending);
    srv.set_now(T0 + 180);
    srv.admit(&admin, &other, "personal", "bob", "bob-box");
    assert!(matches!(team_join::check_approval(None, &fresh, "personal").unwrap(), Some(Approval::Waiting { .. })),
        "nothing signs the new key but an explicit approve");
    assert_eq!(member_status(&srv, &fresh, "personal", "/sessions"), 403);
}

#[test]
fn a_listed_machines_key_cannot_be_replaced_through_a_join_and_the_real_device_keeps_working() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("steal-admin");
    let cfg = sandbox("steal-member");
    srv.create(&admin, "personal", &["alice"]);
    srv.admit(&admin, &cfg, "personal", "alice", "laptop");
    srv.set_now(T0 + 60);
    let thief = sandbox("steal-thief");
    let e = team_join::join(&srv.base, "personal", "alice", "laptop", &srv.code("personal"), &thief).unwrap_err();
    assert!(e.contains("409") && e.contains("machine name already in the team"), "{e}");
    assert!(!credential_path(&thief, "personal").exists(), "nothing is saved for a refused join");
    let c = team_join::load(&credential_path(&cfg, "personal")).unwrap();
    assert!(fetch_and_verify(&srv.base, &c.token, &c.machine, "personal", &trust_of(&c).unwrap()).is_some(),
        "the real device keeps working");
    let roster = run_admin(&srv, &admin, Command::Roster { name: "personal".into() }).unwrap();
    assert!(roster.contains("pending: none"), "no request was recorded: {roster}");
}

#[test]
fn a_removed_member_is_blocked_until_invited_again() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("rm-admin");
    let bcfg = sandbox("rm-bob");
    srv.create(&admin, "personal", &["alice", "bob"]);
    srv.admit(&admin, &bcfg, "personal", "bob", "bob-box");
    let text = run_admin(&srv, &admin, Command::Remove { name: "personal".into(), member: "bob".into() }).unwrap();
    assert!(text.contains("1 device(s)"), "{text}");
    assert_eq!(member_status(&srv, &bcfg, "personal", "/sessions"), 403, "out at once");

    srv.set_now(T0 + 60);
    let e = team_join::join(&srv.base, "personal", "bob", "bob-box-2", &srv.code("personal"), &sandbox("rm-bob-2")).unwrap_err();
    assert!(e.contains("403") && e.contains("not a member"), "a removed member is refused at the door: {e}");

    run_admin(&srv, &admin, Command::Invite { name: "personal".into(), member: "bob".into() }).unwrap();
    srv.set_now(T0 + 120);
    let j = team_join::join(&srv.base, "personal", "bob", "bob-box-2", &srv.code("personal"), &sandbox("rm-bob-3")).unwrap();
    assert!(j.pending, "invited again, a new key asks like any other");
}

#[test]
fn roster_and_decline_need_the_admin_token_and_a_declined_key_stays_out() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("dec-admin");
    let cfg = sandbox("dec-member");
    srv.create(&admin, "personal", &["alice"]);
    srv.join(&cfg, "personal", "alice", "laptop");
    let member_token = team_join::load(&credential_path(&cfg, "personal")).unwrap().token;
    for token in ["not-the-admin-token-00000000000000000000".to_string(), member_token] {
        let e = sesync::team_cli::run(&Command::Roster { name: "personal".into() }, &srv.base, &token, &admin).unwrap_err();
        assert!(e.contains("403"), "{e}");
        let decline = Command::Decline { name: "personal".into(), member: "alice".into(), machine: "laptop".into() };
        let e = sesync::team_cli::run(&decline, &srv.base, &token, &admin).unwrap_err();
        assert!(e.contains("403"), "{e}");
    }
    let text = run_admin(&srv, &admin, Command::Decline { name: "personal".into(), member: "alice".into(), machine: "laptop".into() }).unwrap();
    assert!(text.contains("declined"), "{text}");
    srv.set_now(T0 + 60);
    let e = team_join::join(&srv.base, "personal", "alice", "laptop", &srv.code("personal"), &cfg).unwrap_err();
    assert!(e.contains("removed from the team"), "{e}");
}

#[test]
fn approve_with_a_wrong_fingerprint_signs_nothing() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("fp-admin");
    let cfg = sandbox("fp-member");
    srv.create(&admin, "personal", &["alice"]);
    srv.join(&cfg, "personal", "alice", "laptop");
    let before = list_version(&srv, "personal");
    let wrong = Command::Approve { name: "personal".into(), member: "alice".into(), machine: "laptop".into(),
                                   fingerprint: Some("0000-0000-0000-0000".into()) };
    let e = run_admin(&srv, &admin, wrong).unwrap_err();
    assert!(e.contains("nothing was signed"), "{e}");
    let missing = Command::Approve { name: "personal".into(), member: "alice".into(), machine: "desk".into(), fingerprint: None };
    let e = run_admin(&srv, &admin, missing).unwrap_err();
    assert!(e.contains("no join request"), "{e}");
    assert_eq!(list_version(&srv, "personal"), before, "no list was published");
    assert!(matches!(team_join::check_approval(None, &cfg, "personal").unwrap(), Some(Approval::Waiting { .. })));
}

#[test]
fn the_admins_own_key_cannot_be_revoked_or_removed() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("own-admin");
    srv.create(&admin, "personal", &["alice"]);
    // A machine joins with the admin key itself (its device key directory is the admin one), so
    // the server names the admin entry "alice / admin-box". The key is listed, so it binds at once.
    let j = team_join::join(&srv.base, "personal", "alice", "admin-box", &srv.code("personal"), &admin.join("admin")).unwrap();
    assert!(!j.pending, "a listed key re-authenticates");
    let before = list_version(&srv, "personal");
    let e = run_admin(&srv, &admin, Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "admin-box".into() }).unwrap_err();
    assert!(e.contains("admin's own key"), "{e}");
    let e = run_admin(&srv, &admin, Command::Remove { name: "personal".into(), member: "alice".into() }).unwrap_err();
    assert!(e.contains("admin's own key"), "{e}");
    assert_eq!(list_version(&srv, "personal"), before, "nothing was signed");
}

#[test]
fn the_daemon_says_once_that_a_join_waits_and_starts_syncing_once_approved() {
    let srv = TeamServer::start(wall_now());
    let admin = sandbox("dw-admin");
    let cfg = sandbox("dw-member");
    srv.create(&admin, "personal", &["alice"]);
    let joined = team_join::join(&srv.base, "personal", "alice", "new-box", &srv.code("personal"), &cfg).unwrap();
    let run = |passes: &str| {
        let out = sesync_cmd(&sandbox("dw-bin"), &cfg)
            .arg("--watch")
            .env("SESYNC_PASSES", passes)
            .env("SESYNC_POLL", "0")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{stderr}");
        stderr
    };
    let stderr = run("3");
    let line = format!("team [personal]: waiting for the admin to approve new-box (fingerprint {})", joined.fingerprint);
    assert_eq!(stderr.matches(&line).count(), 1, "said once per run, never an error storm: {stderr}");
    assert!(!stderr.contains("cannot renew"), "{stderr}");

    srv.approve(&admin, "personal", "alice", "new-box", Some(&joined.fingerprint));
    let stderr = run("1");
    assert!(stderr.contains("team [personal]: approved by the admin"), "{stderr}");
    assert!(stderr.contains("team pull [personal]: 0 shown"), "the approved team is pulled in the same step: {stderr}");
    assert!(trust_of(&team_join::load(&credential_path(&cfg, "personal")).unwrap()).is_some(), "the list is stored");
}
