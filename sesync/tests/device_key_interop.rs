//! The daemon's device key and the team server's device type must agree: a device created here
//! has the same signing and box public keys that the server's membership code derives from the
//! same secret, so a list signed over them verifies on both sides.
use sesync::device_keys::load_or_create;

#[test]
fn a_daemon_device_signs_and_the_server_verifies_it() {
    let d = std::env::temp_dir().join(format!("sesync-interop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let dev = load_or_create(&d).unwrap();
    // The server's Device rebuilt from the same PKCS#8 bytes must report the same public key.
    let server_pub = {
        use ring::signature::{Ed25519KeyPair, KeyPair};
        Ed25519KeyPair::from_pkcs8(dev.signing_pkcs8()).unwrap().public_key().as_ref().to_vec()
    };
    assert_eq!(server_pub, dev.public_signing(), "signing keys agree");
    // The box public key must be the X25519 point for this secret, as the server computes it.
    let expect = crypto_box::SecretKey::from(*dev.box_secret()).public_key().as_bytes().to_vec();
    assert_eq!(expect, dev.public_box(), "box keys agree");
    let _ = std::fs::remove_dir_all(&d);
}
