//! The phone decides which machines see each other (docs/BRIDGE.md, "Machines"). On the machine that grants a link
//! (B): `machines.grant` makes a `machine` device for the other machine, and `devices.revoke` removes one. On the
//! machine that sees the other (A): `machines.accept` saves B in `machines.json` with the grant's device id and tries
//! it at once; `machines.list` lists the saved machines (never their tokens); `machines.forget` removes one and
//! returns its grant id so the phone can revoke it on B. A connection authenticated with a `machine` device is refused
//! all five (`rpc::dispatch`): a machine cannot mint access for another machine.

use super::devices::{self, Kind, Refusal};
use super::e2ee;
use super::keys;
use super::offer;
use super::rpc::{Ctx, Failure};
use crate::machines::{client, store};
use serde_json::{json, Value};
use std::time::Duration;

/// How long `machines.accept` waits for the other machine to answer `status.get`. The save stands either way.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn text_param(params: &Value, key: &str) -> Result<String, Failure> {
    params[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or(("bad_request", format!("{key} must be a non-empty string")))
}

fn unavailable(message: String) -> Failure {
    ("unavailable", message)
}

/// `machines.grant {label}` on B: a `machine` device labelled `label` (the granted machine's name for this one), and
/// this bridge's public key, which the granted machine pins as it would from a pairing offer.
pub fn grant(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let label = text_param(params, "label")?;
    let keys = keys::load_or_create(&ctx.dir.join("server.key")).map_err(unavailable)?;
    let (device, token) = devices::create(&ctx.dir, &label, Kind::Machine, devices::now_secs()).map_err(unavailable)?;
    Ok(json!({ "deviceId": device.id, "deviceToken": token, "publicKeyB64": keys.public_key_b64() }))
}

/// `devices.revoke {deviceId}` on B: removes a `machine` device. A phone is refused: `sesync bridge revoke` removes one.
pub fn revoke_device(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let id = text_param(params, "deviceId")?;
    match devices::revoke_machine(&ctx.dir, &id) {
        Ok(_) => Ok(json!({ "revoked": true })),
        Err(Refusal::Unknown) => Err(("not_found", format!("no paired device with id {id}"))),
        Err(Refusal::Phone) => Err(("forbidden", "only a machine device can be revoked this way".into())),
        Err(Refusal::Failed(message)) => Err(unavailable(message)),
    }
}

/// `machines.accept {name, endpoint, deviceToken, publicKeyB64, grantDeviceId}` on A: saves the machine under `name`
/// (replacing a machine of that name), then asks it for `status.get` over the hybrid handshake. The answer is
/// `answers`, and `error` says why when it is false; a machine that does not answer stays saved.
pub fn accept(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let name = text_param(params, "name")?;
    store::check_name(&name).map_err(|m| ("bad_request", m))?;
    let endpoint = text_param(params, "endpoint")?;
    offer::check_endpoint(&endpoint).map_err(|m| ("bad_request", m))?;
    let public_key_b64 = text_param(params, "publicKeyB64")?;
    if e2ee::parse_client_key(&public_key_b64).is_none() {
        return Err(("bad_request", "publicKeyB64 is not a 32-byte key".into()));
    }
    let machine = store::Machine {
        name: name.clone(),
        endpoint,
        device_token: text_param(params, "deviceToken")?,
        public_key_b64,
        grant_device_id: Some(text_param(params, "grantDeviceId")?),
        // A local override set on this machine is kept by `store::put`; the phone does not know it.
        local_endpoint: None,
    };
    store::put(&ctx.machines, machine.clone()).map_err(|m| ("unavailable", m))?;
    let saved = store::load(&ctx.machines).map_err(unavailable)?.into_iter().find(|m| m.name == name).unwrap_or(machine);
    Ok(match probe(&saved) {
        Ok(()) => json!({ "name": name, "answers": true }),
        Err(reason) => json!({ "name": name, "answers": false, "error": reason }),
    })
}

/// Asks `machine` for `status.get` over a fresh hybrid connection, within `PROBE_TIMEOUT`. The dispatch runs on the
/// bridge's blocking pool (`server.rs`), so the check gets its own current-thread runtime there: it never blocks the
/// bridge's reactor, and no task of the bridge waits on it. Must not be called from inside an async task.
fn probe(machine: &store::Machine) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the check: {e}"))?;
    runtime.block_on(async {
        let check = async {
            let (mut writer, mut reader) = client::connect(machine).await?;
            client::call(&mut writer, &mut reader, "status.get", json!({})).await.map(|_| ())
        };
        tokio::time::timeout(PROBE_TIMEOUT, check)
            .await
            .unwrap_or_else(|_| Err(format!("{} did not answer within {}s", machine.name, PROBE_TIMEOUT.as_secs())))
    })
}

/// `machines.list` on A: each saved machine's name, endpoint and grant id. Tokens and keys are never listed.
pub fn list(ctx: &Ctx) -> Result<Value, Failure> {
    let machines = store::load(&ctx.machines).map_err(unavailable)?;
    let rows: Vec<Value> = machines
        .iter()
        .map(|m| json!({ "name": m.name, "endpoint": m.endpoint, "grantDeviceId": m.grant_device_id }))
        .collect();
    Ok(json!({ "machines": rows }))
}

/// `machines.forget {name}` on A: removes the machine and returns its grant id (null for a hand-made pairing).
pub fn forget(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let name = text_param(params, "name")?;
    let removed = store::remove(&ctx.machines, &name)
        .map_err(unavailable)?
        .ok_or_else(|| ("not_found", format!("no machine named {name:?}")))?;
    Ok(json!({ "grantDeviceId": removed.grant_device_id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::testkit::{ctx_with, sandbox};
    use std::path::Path;

    fn ctx_in(dir: &Path) -> Ctx {
        let mut ctx = ctx_with(vec![], &dir.join("bridge"));
        ctx.machines = dir.join("machines.json");
        ctx
    }

    /// A loopback port nothing listens on: a machine there refuses at once.
    fn closed_endpoint() -> String {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("ws://127.0.0.1:{port}")
    }

    const KEY: &str = "zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk=";

    fn accept_params(name: &str, endpoint: &str, grant: &str) -> Value {
        json!({ "name": name, "endpoint": endpoint, "deviceToken": "tok", "publicKeyB64": KEY, "grantDeviceId": grant })
    }

    #[test]
    fn a_grant_is_a_machine_device_with_this_bridges_key_and_the_token_authenticates_as_one() {
        let dir = sandbox("grants-grant");
        let ctx = ctx_in(&dir);
        let reply = grant(&json!({ "label": "A" }), &ctx).unwrap();
        let token = reply["deviceToken"].as_str().unwrap();
        assert_eq!(reply["publicKeyB64"], keys::load_or_create(&ctx.dir.join("server.key")).unwrap().public_key_b64());
        assert_eq!(devices::authenticate(&ctx.dir, token), Some(Kind::Machine));
        let id = reply["deviceId"].as_str().unwrap();
        assert_eq!(devices::list(&ctx.dir).unwrap().iter().find(|d| d.id == id).unwrap().label, "A");
        assert_eq!(grant(&json!({ "label": "" }), &ctx).unwrap_err().0, "bad_request");
    }

    #[test]
    fn revoke_refuses_a_phone_and_an_unknown_id_and_revokes_a_grant() {
        let dir = sandbox("grants-revoke");
        let ctx = ctx_in(&dir);
        let (phone, phone_token) = devices::create(&ctx.dir, "phone", Kind::Phone, 1).unwrap();
        let granted = grant(&json!({ "label": "A" }), &ctx).unwrap();
        assert_eq!(revoke_device(&json!({ "deviceId": phone.id }), &ctx).unwrap_err().0, "forbidden");
        assert_eq!(revoke_device(&json!({ "deviceId": "nope" }), &ctx).unwrap_err().0, "not_found");
        assert_eq!(devices::authenticate(&ctx.dir, &phone_token), Some(Kind::Phone), "the phone is still paired");
        let id = granted["deviceId"].as_str().unwrap();
        assert_eq!(revoke_device(&json!({ "deviceId": id }), &ctx).unwrap(), json!({ "revoked": true }));
        assert_eq!(devices::authenticate(&ctx.dir, granted["deviceToken"].as_str().unwrap()), None);
        assert_eq!(revoke_device(&json!({ "deviceId": id }), &ctx).unwrap_err().0, "not_found", "twice is not_found");
    }

    #[test]
    fn accept_saves_the_machine_even_when_it_does_not_answer_and_says_so() {
        let dir = sandbox("grants-accept-down");
        let ctx = ctx_in(&dir);
        let endpoint = closed_endpoint();
        let reply = accept(&accept_params("test-box", &endpoint, "g-1"), &ctx).unwrap();
        assert_eq!(reply["name"], "test-box");
        assert_eq!(reply["answers"], false);
        assert!(reply["error"].as_str().unwrap().contains("cannot reach"), "{reply}");
        let saved = store::load(&ctx.machines).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].endpoint, endpoint);
        assert_eq!(saved[0].grant_device_id.as_deref(), Some("g-1"));
    }

    #[test]
    fn accept_refuses_a_bad_name_endpoint_or_key_and_saves_nothing() {
        let dir = sandbox("grants-accept-bad");
        let ctx = ctx_in(&dir);
        let good = accept_params("ok", "ws://127.0.0.1:9", "g");
        let bad = [
            json!({ "name": "has space", "endpoint": "ws://127.0.0.1:9", "deviceToken": "t", "publicKeyB64": KEY, "grantDeviceId": "g" }),
            json!({ "name": "ok", "endpoint": "http://127.0.0.1:9", "deviceToken": "t", "publicKeyB64": KEY, "grantDeviceId": "g" }),
            json!({ "name": "ok", "endpoint": "ws://127.0.0.1:9", "deviceToken": "t", "publicKeyB64": "not-a-key", "grantDeviceId": "g" }),
            json!({ "name": "ok", "endpoint": "ws://127.0.0.1:9", "deviceToken": "t", "publicKeyB64": KEY }),
            json!({ "name": "ok", "endpoint": "ws://127.0.0.1:9", "publicKeyB64": KEY, "grantDeviceId": "g" }),
        ];
        for params in &bad {
            assert_eq!(accept(params, &ctx).unwrap_err().0, "bad_request", "{params}");
        }
        assert!(store::load(&ctx.machines).unwrap().is_empty(), "a refused accept saves nothing");
        assert!(accept(&good, &ctx).is_ok());
    }

    #[test]
    fn forget_returns_the_grant_id_and_list_never_shows_a_token() {
        let dir = sandbox("grants-forget");
        let ctx = ctx_in(&dir);
        let endpoint = closed_endpoint();
        accept(&accept_params("granted", &endpoint, "g-9"), &ctx).unwrap();
        store::add(&ctx.machines, "by-hand", &crate::bridge::offer::Offer {
            endpoint: "ws://10.10.20.7:6768".into(),
            device_token: "secret-by-hand-token".into(),
            public_key_b64: KEY.into(),
        })
        .unwrap();

        let listed = list(&ctx).unwrap();
        let rows = listed["machines"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "granted");
        assert_eq!(rows[0]["endpoint"], endpoint.as_str(), "{listed}");
        assert_eq!(rows[0]["grantDeviceId"], "g-9");
        assert_eq!(rows[1]["grantDeviceId"], Value::Null, "a hand-made entry has no grant");
        assert!(!listed.to_string().contains("secret-by-hand-token"), "{listed}");
        assert!(!listed.to_string().contains("publicKeyB64"), "{listed}");

        assert_eq!(forget(&json!({ "name": "granted" }), &ctx).unwrap(), json!({ "grantDeviceId": "g-9" }));
        assert_eq!(forget(&json!({ "name": "by-hand" }), &ctx).unwrap(), json!({ "grantDeviceId": null }));
        assert_eq!(forget(&json!({ "name": "granted" }), &ctx).unwrap_err().0, "not_found");
        assert_eq!(list(&ctx).unwrap(), json!({ "machines": [] }));
    }
}
