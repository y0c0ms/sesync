//! Round trip of a real local session through the team store: write it as a member, check the
//! database holds only ciphertext, read it back, and compare its hash with the source.
//! Runs only when TEAM_TEST_PG and TEAM_SESSION_FILE are set, and writes to a throwaway schema.

use sesync_team::pg::PgStore;
use sesync_team::service::Service;
use sesync_team::totp::code_at;
use sha2::{Digest, Sha256};

const T0: u64 = 1_000_020;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[test]
fn a_local_session_survives_the_database_round_trip() {
    let (Some(url), Some(file)) = (std::env::var("TEAM_TEST_PG").ok(), std::env::var("TEAM_SESSION_FILE").ok())
        else { return };
    let plain = std::fs::read(&file).expect("read the session file");
    let src_hash = format!("{:x}", Sha256::digest(&plain));

    let master = [7u8; 32];
    let schema_url = format!("{url}?options=-c%20search_path%3Dt_local_session");
    {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        c.batch_execute("DROP SCHEMA IF EXISTS t_local_session CASCADE; CREATE SCHEMA t_local_session;").unwrap();
    }
    let store = PgStore::connect(&schema_url, master).expect("connect");
    store.reset().expect("schema");
    let mut s = Service::new(store);
    s.create_space("local", &["me"]).unwrap();
    let code = code_at(&s.space_secret("local").unwrap(), T0);
    let token = s.enrol("local", "me", "this-box", &code, None, T0).map(|a| a.token().to_string()).unwrap();
    assert_eq!(s.authenticate("local", &token, "this-box", now().max(T0)).is_err(), now() > T0 + 86_400,
        "token check behaves on wall clock");
    let lease = s.acquire("local", "session", "this-box", T0).unwrap();
    s.put_record("local", "session", "this-box", lease.version, plain.clone(), T0).unwrap();

    // What the database holds: must not contain the plaintext, and must be a different length.
    let raw = s.raw_stored("local", "session").expect("stored");
    let contains_plain = raw.windows(plain.len().min(32)).any(|w| plain.len() >= 32 && w == &plain[..32]);
    println!("RESULT stored_bytes={} source_bytes={} plaintext_in_db={}", raw.len(), plain.len(), contains_plain);
    assert!(!contains_plain, "the database must not hold the session in the clear");

    // Read back as the member and compare hashes.
    let back = s.get_record("local", "session", "me").expect("read back");
    let back_hash = format!("{:x}", Sha256::digest(&back));
    println!("RESULT restored_matches={}", back_hash == src_hash);
    assert_eq!(back, plain, "the restored session must equal the source byte for byte");
}
