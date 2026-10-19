//! A device's keys live in its config directory: created once, owner-only, and reloaded on later
//! runs so the device keeps its identity. A loose key file is refused, and nothing prints a key.
use sesync::device_keys::{load_or_create, DeviceFile};
use std::os::unix::fs::PermissionsExt;

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-devkeys-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_first_run_creates_keys_and_a_later_run_reloads_the_same_device() {
    let d = dir("reload");
    let first = load_or_create(&d).unwrap();
    let second = load_or_create(&d).unwrap();
    assert_eq!(first.public_signing(), second.public_signing(), "the same device on every run");
    assert_eq!(first.public_box(), second.public_box());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_key_file_is_owner_only() {
    let d = dir("mode");
    let _ = load_or_create(&d).unwrap();
    let mode = std::fs::metadata(d.join("device.key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_loose_key_file_is_refused() {
    let d = dir("loose");
    let _ = load_or_create(&d).unwrap();
    std::fs::set_permissions(d.join("device.key"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = load_or_create(&d).err().expect("refused");
    assert!(err.contains("readable by other users"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_damaged_key_file_is_refused_without_echoing_it() {
    let d = dir("damaged");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("device.key"), "signing=not-hex-secretvalue\n").unwrap();
    std::fs::set_permissions(d.join("device.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = load_or_create(&d).err().expect("refused");
    assert!(!err.contains("secretvalue"), "the key is never echoed: {err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_device_file_round_trips() {
    let d = dir("roundtrip");
    let made = load_or_create(&d).unwrap();
    let back = DeviceFile::read(&d.join("device.key")).unwrap();
    assert_eq!(back.public_signing(), made.public_signing());
    let _ = std::fs::remove_dir_all(&d);
}
