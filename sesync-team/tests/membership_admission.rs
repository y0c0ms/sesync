//! Team membership and revocation, as docs/ARCHITECTURE.md "Team membership and revocation"
//! states it: only the admin's signed list admits a device; the team code lets a device ask to
//! join and re-authenticate its own listed key; a new key waits for the admin; a listed machine's
//! key is never replaced through a join; a revoked key and a removed member are refused at the
//! door; names and requests are admin-only. Run on the real router over the in-memory store.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, enrol_message, fingerprint, Device, MemberList};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{Admission, JoinRequest, MemoryStore, Service, ServiceError, Store,
                           MAX_REQUESTS_PER_MEMBER, MAX_REQUESTS_PER_TEAM, REQUEST_TTL_SECS};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "admin-token-for-tests-0123456789abcdef";
const SPACE: &str = "personal";
/// Mid-step, so a code never straddles a 30-second boundary.
const T0: u64 = 1_000_020;

struct Team {
    app: axum::Router,
    state: Arc<AppState<MemoryStore>>,
    now: Arc<AtomicU64>,
    admin: Device,
    version: u64,
    entries: Vec<Vec<u8>>,
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

async fn post(app: &axum::Router, uri: &str, auth: Option<&str>, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(a) = auth { b = b.header("authorization", a); }
    let resp = app.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

impl Team {
    /// A team with a pinned admin key and list v1 naming only the admin key.
    async fn create(members: &[&str]) -> Team {
        let now = Arc::new(AtomicU64::new(T0));
        let n = now.clone();
        let clock: Clock = Arc::new(move || n.load(Ordering::SeqCst));
        let state = Arc::new(AppState { service: Mutex::new(Service::new(MemoryStore::default())), clock,
            metrics: Default::default(), admin_token: Some(ADMIN_TOKEN.into()) });
        let app = server::router(state.clone());
        let admin = Device::generate();
        let entries = vec![device_entry(&admin)];
        let list = MemberList::new(1, entries.clone()).sign(&admin);
        let (st, j) = post(&app, "/admin/team", Some(&format!("Bearer {ADMIN_TOKEN}")), serde_json::json!({
            "space": SPACE, "members": members, "admin_public": hex(&admin.public_key()),
            "devices": [hex(&entries[0])], "signed_list": hex(&list.to_bytes()),
        })).await;
        assert_eq!(st, StatusCode::OK, "{j}");
        Team { app, state, now, admin, version: 1, entries }
    }

    fn set_now(&self, t: u64) { self.now.store(t, Ordering::SeqCst); }

    fn code(&self) -> String {
        let svc = self.state.service.lock().unwrap();
        sesync_team::totp::code_at(&svc.space_secret(SPACE).unwrap(), self.now.load(Ordering::SeqCst))
    }

    async fn join(&self, member: &str, machine: &str, device: &Device) -> (StatusCode, serde_json::Value) {
        self.join_with_code(member, machine, device, &self.code()).await
    }

    /// A join as a real client sends it: the entry and the device's own signature over it.
    async fn join_with_code(&self, member: &str, machine: &str, device: &Device, code: &str) -> (StatusCode, serde_json::Value) {
        let entry = device_entry(device);
        let proof = device.sign(&enrol_message(SPACE, member, machine, code, &entry));
        post(&self.app, "/enrol", None, serde_json::json!({
            "space": SPACE, "member": member, "machine": machine, "code": code,
            "device": hex(&entry), "proof": hex(&proof)})).await
    }

    /// Sign and publish the next list: the current entries with `add` and without `drop`.
    async fn publish(&mut self, add: &[&Device], drop: &[&Device], removed_members: &[&str]) -> (StatusCode, serde_json::Value) {
        let mut entries: Vec<Vec<u8>> = self.entries.iter()
            .filter(|e| !drop.iter().any(|d| &device_entry(d) == *e)).cloned().collect();
        entries.extend(add.iter().map(|d| device_entry(d)));
        let signed = MemberList::new(self.version + 1, entries.clone()).sign(&self.admin);
        let r = post(&self.app, "/admin/list", Some(&format!("Bearer {ADMIN_TOKEN}")), serde_json::json!({
            "space": SPACE, "signed_list": hex(&signed.to_bytes()), "removed_members": removed_members})).await;
        if r.0 == StatusCode::OK {
            self.version += 1;
            self.entries = signed.list.devices;
        }
        r
    }

    async fn member_call(&self, uri: &str, token: &str, machine: &str) -> StatusCode {
        let mut body = serde_json::json!({"space": SPACE, "machine": machine});
        if uri.ends_with("read") || uri.starts_with("/lease") {
            body["conversation"] = serde_json::json!("ses_x");
        }
        post(&self.app, uri, Some(&format!("Bearer {token}")), body).await.0
    }

    async fn roster(&self) -> serde_json::Value {
        let (st, j) = post(&self.app, "/admin/roster", Some(&format!("Bearer {ADMIN_TOKEN}")),
            serde_json::json!({"space": SPACE})).await;
        assert_eq!(st, StatusCode::OK, "{j}");
        j
    }
}

fn token_of(j: &serde_json::Value) -> String { j["token"].as_str().expect("a token").to_string() }

#[test]
fn a_fingerprint_is_eight_bytes_of_sha256_in_four_groups() {
    let entry = [7u8; 64];
    let fp = fingerprint(&entry);
    assert_eq!(fp.len(), 19, "{fp}");
    let groups: Vec<&str> = fp.split('-').collect();
    assert_eq!(groups.len(), 4, "{fp}");
    assert!(groups.iter().all(|g| g.len() == 4 && g.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())), "{fp}");
    let digest = ring::digest::digest(&ring::digest::SHA256, &entry);
    let want: String = digest.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(fp.replace('-', ""), want);
    assert_ne!(fingerprint(&[8u8; 64]), fp, "another key, another fingerprint");
}

#[tokio::test]
async fn a_new_key_waits_with_no_list_and_every_data_route_refuses_it_until_approved() {
    let mut t = Team::create(&["alice"]).await;
    let laptop = Device::generate();
    let (st, j) = t.join("alice", "laptop", &laptop).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{j}");
    assert_eq!(j["status"], "pending");
    assert_eq!(j["fingerprint"], fingerprint(&device_entry(&laptop)));
    assert_eq!(j["admin_public"], hex(&t.admin.public_key()), "the admin key is pinned now");
    assert!(j.get("signed_list").is_none() && j.get("list_version").is_none(), "no roster for a pending device: {j}");
    let token = token_of(&j);
    for uri in ["/list", "/sessions", "/record/sealed/read", "/lease/acquire"] {
        assert_eq!(t.member_call(uri, &token, "laptop").await, StatusCode::FORBIDDEN, "{uri} refuses a pending device");
    }
    let (st, _) = post(&t.app, "/renew", Some(&format!("Bearer {token}")), serde_json::json!({"space": SPACE, "machine": "laptop"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a pending device cannot renew either");

    // Nothing signs it but the admin: a later publish that does not add it leaves it waiting.
    let (st, _) = t.publish(&[], &[], &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(t.member_call("/list", &token, "laptop").await, StatusCode::FORBIDDEN, "still pending");

    // The admin signs the key in: the same token now works, and the request is gone.
    let (st, j) = t.publish(&[&laptop], &[], &[]).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(t.member_call("/list", &token, "laptop").await, StatusCode::OK, "approved: the list is served");
    assert_eq!(t.member_call("/sessions", &token, "laptop").await, StatusCode::OK);
    let r = t.roster().await;
    assert!(r["pending"].as_array().unwrap().is_empty(), "approval answers the request: {r}");
    assert!(r["listed"].as_array().unwrap().iter().any(|d| d["machine"] == "laptop" && d["member"] == "alice"), "{r}");
}

#[tokio::test]
async fn a_listed_key_joining_again_is_re_authenticated_with_the_list() {
    let mut t = Team::create(&["alice"]).await;
    let laptop = Device::generate();
    t.join("alice", "laptop", &laptop).await;
    t.publish(&[&laptop], &[], &[]).await;
    t.set_now(T0 + 60);
    let (st, j) = t.join("alice", "laptop", &laptop).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["status"], "listed");
    assert_eq!(j["list_version"], 2);
    assert!(j["signed_list"].as_str().is_some());
}

#[tokio::test]
async fn a_revoked_device_is_refused_at_once_and_its_key_never_joins_again() {
    let mut t = Team::create(&["alice"]).await;
    let laptop = Device::generate();
    let (_, j) = t.join("alice", "laptop", &laptop).await;
    let token = token_of(&j);
    t.publish(&[&laptop], &[], &[]).await;
    assert_eq!(t.member_call("/list", &token, "laptop").await, StatusCode::OK);

    let (st, _) = t.publish(&[], &[&laptop], &[]).await;
    assert_eq!(st, StatusCode::OK);
    for uri in ["/list", "/sessions", "/record/sealed/read"] {
        assert_eq!(t.member_call(uri, &token, "laptop").await, StatusCode::FORBIDDEN, "{uri}: revoked at once");
    }

    // The same key at the door: refused, no token.
    t.set_now(T0 + 60);
    let (st, j) = t.join("alice", "laptop", &laptop).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{j}");
    assert_eq!(j["error"], "this device was removed from the team");
    assert!(j.get("token").is_none());

    // A new key from the same person only asks; nothing signs it without the admin.
    t.set_now(T0 + 120);
    let fresh = Device::generate();
    let (st, j) = t.join("alice", "laptop", &fresh).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{j}");
    let fresh_token = token_of(&j);
    t.publish(&[], &[], &[]).await;
    assert_eq!(t.member_call("/list", &fresh_token, "laptop").await, StatusCode::FORBIDDEN);
    let r = t.roster().await;
    assert_eq!(r["pending"][0]["fingerprint"], fingerprint(&device_entry(&fresh)), "{r}");
}

#[tokio::test]
async fn a_listed_machines_key_cannot_be_replaced_through_a_join() {
    let mut t = Team::create(&["alice", "eve"]).await;
    let laptop = Device::generate();
    let (_, j) = t.join("alice", "laptop", &laptop).await;
    let token = token_of(&j);
    t.publish(&[&laptop], &[], &[]).await;

    // Someone holding the code presents another key under the listed machine's name.
    t.set_now(T0 + 60);
    let thief = Device::generate();
    for who in ["alice", "eve"] {
        let (st, j) = t.join(who, "laptop", &thief).await;
        assert_eq!(st, StatusCode::CONFLICT, "{j}");
        assert_eq!(j["error"], "machine name already in the team");
        t.set_now(T0 + 90);
    }
    // The real device keeps working, nothing waits for approval, and the binding is unchanged.
    assert_eq!(t.member_call("/list", &token, "laptop").await, StatusCode::OK, "the real device is untouched");
    let r = t.roster().await;
    assert!(r["pending"].as_array().unwrap().is_empty(), "no request was recorded: {r}");
    let bound = r["listed"].as_array().unwrap().iter().find(|d| d["machine"] == "laptop").unwrap();
    assert_eq!(bound["fingerprint"], fingerprint(&device_entry(&laptop)));

    // The listed key under another machine name is refused too.
    t.set_now(T0 + 150);
    let (st, j) = t.join("alice", "laptop-2", &laptop).await;
    assert_eq!(st, StatusCode::CONFLICT, "{j}");
}

#[tokio::test]
async fn a_removed_member_is_refused_until_invited_again() {
    let mut t = Team::create(&["alice", "bob"]).await;
    let bob_box = Device::generate();
    let (_, j) = t.join("bob", "bob-box", &bob_box).await;
    let token = token_of(&j);
    t.publish(&[&bob_box], &[], &[]).await;
    assert_eq!(t.member_call("/sessions", &token, "bob-box").await, StatusCode::OK);

    let (st, j) = t.publish(&[], &[&bob_box], &["bob"]).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(t.member_call("/sessions", &token, "bob-box").await, StatusCode::FORBIDDEN, "out at once");

    t.set_now(T0 + 60);
    let other = Device::generate();
    let (st, j) = t.join("bob", "bob-box-2", &other).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a removed member is refused at the door: {j}");
    assert_eq!(j["error"], "not a member");
    assert!(t.roster().await["pending"].as_array().unwrap().is_empty());

    // Invited again: the old key stays refused, a new key waits for the admin.
    let (st, _) = post(&t.app, "/admin/invite", Some(&format!("Bearer {ADMIN_TOKEN}")),
        serde_json::json!({"space": SPACE, "member": "bob"})).await;
    assert_eq!(st, StatusCode::OK);
    t.set_now(T0 + 120);
    let (st, _) = t.join("bob", "bob-box", &bob_box).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "the revoked key stays revoked");
    t.set_now(T0 + 180);
    let (st, j) = t.join("bob", "bob-box-2", &other).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{j}");
}

#[tokio::test]
async fn removing_a_name_that_is_not_a_member_changes_nothing() {
    let mut t = Team::create(&["alice"]).await;
    let (st, j) = t.publish(&[], &[], &["nobody"]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{j}");
    assert_eq!(t.version, 1, "the list was not stored");
}

#[tokio::test]
async fn roster_and_decline_are_admin_only_and_a_declined_key_is_refused() {
    let t = Team::create(&["alice"]).await;
    let laptop = Device::generate();
    let (_, j) = t.join("alice", "laptop", &laptop).await;
    let token = token_of(&j);
    let body = serde_json::json!({"space": SPACE, "member": "alice", "machine": "laptop"});
    for auth in [None, Some(format!("Bearer {token}")), Some("Bearer nonsense".to_string())] {
        let (st, _) = post(&t.app, "/admin/roster", auth.as_deref(), serde_json::json!({"space": SPACE})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = post(&t.app, "/admin/decline", auth.as_deref(), body.clone()).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
    assert_eq!(t.roster().await["pending"].as_array().unwrap().len(), 1, "a refused decline changes nothing");

    let admin = format!("Bearer {ADMIN_TOKEN}");
    let (st, j) = post(&t.app, "/admin/decline", Some(&admin),
        serde_json::json!({"space": SPACE, "member": "alice", "machine": "laptpo"})).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a typo is not a decline: {j}");
    let (st, j) = post(&t.app, "/admin/decline", Some(&admin), body).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert!(t.roster().await["pending"].as_array().unwrap().is_empty());
    t.set_now(T0 + 60);
    let (st, j) = t.join("alice", "laptop", &laptop).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{j}");
    assert_eq!(j["error"], "this device was removed from the team");
}

#[tokio::test]
async fn a_wrong_code_records_nothing_about_the_machine() {
    let mut t = Team::create(&["alice", "eve"]).await;
    let laptop = Device::generate();
    t.join("alice", "laptop", &laptop).await;
    t.publish(&[&laptop], &[], &[]).await;
    // A wrong code under alice's machine name, as eve, signed with eve's own key.
    let (st, _) = t.join_with_code("eve", "laptop", &Device::generate(), "000000").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // Alice's own re-authentication is unaffected: the machine is still hers.
    t.set_now(T0 + 60);
    let (st, j) = t.join("alice", "laptop", &laptop).await;
    assert_eq!(st, StatusCode::OK, "{j}");
}

#[test]
fn join_requests_are_limited_per_member_and_per_team_and_expire() {
    let admin = Device::generate();
    let mut svc = Service::new(MemoryStore::default());
    let members: Vec<String> = (0..10).map(|i| format!("m{i}")).collect();
    let refs: Vec<&str> = members.iter().map(String::as_str).collect();
    svc.create_space(SPACE, &refs).unwrap();
    svc.pin_admin_key(SPACE, &admin.public_key()).unwrap();
    svc.store_trust_list(SPACE, &MemberList::new(1, vec![device_entry(&admin)]).sign(&admin).to_bytes()).unwrap();
    let secret = svc.space_secret(SPACE).unwrap();
    let join = |svc: &mut Service<MemoryStore>, who: &str, machine: &str, at: u64| {
        let code = sesync_team::totp::code_at(&secret, at);
        svc.enrol(SPACE, who, machine, &code, Some(&device_entry(&Device::generate())), at)
    };
    for i in 0..MAX_REQUESTS_PER_MEMBER {
        assert!(matches!(join(&mut svc, "m0", &format!("m0-box{i}"), T0), Ok(Admission::Pending { .. })));
    }
    assert!(matches!(join(&mut svc, "m0", "m0-box-extra", T0), Err(ServiceError::Limit(_))), "per member");
    // A request again under one of the member's machine names refreshes it, within the limit.
    assert!(matches!(join(&mut svc, "m0", "m0-box0", T0 + 30), Ok(Admission::Pending { .. })));
    let mut n = MAX_REQUESTS_PER_MEMBER;
    'fill: for m in 1..10 {
        for i in 0..MAX_REQUESTS_PER_MEMBER {
            if n == MAX_REQUESTS_PER_TEAM { break 'fill; }
            join(&mut svc, &format!("m{m}"), &format!("m{m}-box{i}"), T0).unwrap();
            n += 1;
        }
    }
    assert!(matches!(join(&mut svc, "m9", "m9-box9", T0), Err(ServiceError::Limit(_))), "per team");
    assert_eq!(svc.roster(SPACE, T0).unwrap().pending.len(), MAX_REQUESTS_PER_TEAM);
    // Unanswered for 7 days: the requests are gone, and new ones fit again.
    let later = T0 + REQUEST_TTL_SECS;
    assert!(svc.roster(SPACE, later).unwrap().pending.iter().all(|r| r.requested_at > T0), "expired requests are dropped");
    assert!(matches!(join(&mut svc, "m9", "m9-box9", later), Ok(Admission::Pending { .. })));
}

#[test]
fn migration_turns_unlisted_bindings_into_requests_and_keeps_listed_ones() {
    // The state an older server left: two machines bound by enrolment, only one of them signed in.
    let admin = Device::generate();
    let listed = Device::generate();
    let unlisted = Device::generate();
    let mut svc = Service::new(MemoryStore::default());
    svc.create_space(SPACE, &["alice", "bob"]).unwrap();
    svc.pin_admin_key(SPACE, &admin.public_key()).unwrap();
    svc.store_trust_list(SPACE, &MemberList::new(4, vec![device_entry(&admin), device_entry(&listed)]).sign(&admin).to_bytes()).unwrap();
    let secret = svc.space_secret(SPACE).unwrap();
    let mut store = svc.into_store();
    for (machine, member, device) in [("alice-box", "alice", &listed), ("bob-box", "bob", &unlisted)] {
        store.save_device(SPACE, machine, device_entry(device));
        store.save_owner(SPACE, machine, member);
        store.save_enrolled_at(SPACE, machine, T0);
    }
    let key = store.load_token_key(SPACE).unwrap();
    let alice_token = sesync_team::auth::issue_token(&key, "alice", "alice-box", T0 + 86_400);
    let bob_token = sesync_team::auth::issue_token(&key, "bob", "bob-box", T0 + 86_400);

    // The upgraded server starts on that store.
    let mut svc = Service::new(store);
    let roster = svc.roster(SPACE, T0 + 10).unwrap();
    assert_eq!(roster.pending, vec![JoinRequest {
        member: "bob".into(), machine: "bob-box".into(), entry: device_entry(&unlisted), requested_at: T0 + 10 }],
        "the unlisted binding is now a request for the admin");
    assert!(roster.listed.iter().any(|d| d.machine.as_deref() == Some("alice-box")), "the listed binding stays");
    assert!(svc.authenticate(SPACE, &alice_token, "alice-box", T0 + 10).is_ok(), "nobody re-scans anything");
    assert!(svc.authenticate(SPACE, &bob_token, "bob-box", T0 + 10).is_err());
    // Not a tombstone: the admin can approve it, and then the same token works.
    let v5 = MemberList::new(5, vec![device_entry(&admin), device_entry(&listed), device_entry(&unlisted)]).sign(&admin);
    svc.publish_trust_list(SPACE, &v5.to_bytes(), &[], T0 + 20).unwrap();
    assert_eq!(svc.authenticate(SPACE, &bob_token, "bob-box", T0 + 30).unwrap(), "bob");
    let code = sesync_team::totp::code_at(&secret, T0 + 60);
    assert!(matches!(svc.enrol(SPACE, "bob", "bob-box", &code, Some(&device_entry(&unlisted)), T0 + 60),
        Ok(Admission::Listed(_))));
}

#[test]
fn requests_and_tombstones_survive_a_restart() {
    let admin = Device::generate();
    let waiting = Device::generate();
    let revoked = Device::generate();
    let mut svc = Service::new(MemoryStore::default());
    svc.create_space(SPACE, &["alice"]).unwrap();
    svc.pin_admin_key(SPACE, &admin.public_key()).unwrap();
    svc.store_trust_list(SPACE, &MemberList::new(1, vec![device_entry(&admin), device_entry(&revoked)]).sign(&admin).to_bytes()).unwrap();
    let secret = svc.space_secret(SPACE).unwrap();
    let code = sesync_team::totp::code_at(&secret, T0);
    svc.enrol(SPACE, "alice", "box-1", &code, Some(&device_entry(&waiting)), T0).unwrap();
    svc.publish_trust_list(SPACE, &MemberList::new(2, vec![device_entry(&admin)]).sign(&admin).to_bytes(), &[], T0).unwrap();

    let mut restarted = Service::new(svc.into_store());
    assert_eq!(restarted.roster(SPACE, T0 + 1).unwrap().pending.len(), 1, "the request persists");
    let code = sesync_team::totp::code_at(&secret, T0 + 60);
    assert_eq!(restarted.enrol(SPACE, "alice", "box-2", &code, Some(&device_entry(&revoked)), T0 + 60),
        Err(ServiceError::Removed), "the tombstone persists");
}
