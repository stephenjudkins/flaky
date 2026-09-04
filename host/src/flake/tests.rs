use super::{FlakeRef, hash_flake_dir, lock_inputs, parse_flake_ref};
use std::os::unix::fs::PermissionsExt as _;

fn check(ref_: FlakeRef, dir: &str, attr: &str) {
    assert_eq!(ref_.dir, std::path::PathBuf::from(dir));
    assert_eq!(ref_.attr, attr);
}

#[test]
fn parses_flake_refs() {
    check(
        parse_flake_ref("./samples#write-file").unwrap(),
        "./samples",
        "write-file",
    );
    check(
        parse_flake_ref("./samples").unwrap(),
        "./samples",
        "default",
    );
    check(parse_flake_ref("#foo").unwrap(), ".", "foo");
}

#[test]
fn reads_samples_lock() {
    let text = std::fs::read_to_string("../samples/flake.lock").unwrap();
    let lock: super::Lock = serde_json::from_str(&text).unwrap();
    let inputs = lock_inputs(&lock).unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].0, "nixpkgs");
    assert_eq!(inputs[0].1.owner, "NixOS");
    assert_eq!(inputs[0].1.repo, "nixpkgs");
    assert_eq!(inputs[0].1.rev, "2c423e03bbafcff28bfadc6781a4a8257f205cb5");
    assert_eq!(
        inputs[0].1.nar_hash,
        "sha256-dt4WdcvsA8/RCe+VZZwqU0X+XMM3wBbGCWA0/sFWzGo="
    );
}

#[test]
fn flake_dir_hash_is_order_and_content_sensitive() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("flake.nix"), "{ }").unwrap();
    std::fs::write(dir.join("flake.lock"), "{}").unwrap();
    let base = hash_flake_dir(dir).unwrap();
    assert_eq!(base, hash_flake_dir(dir).unwrap());

    std::fs::write(dir.join("flake.nix"), "{ } ").unwrap();
    assert_ne!(base, hash_flake_dir(dir).unwrap());

    std::fs::write(dir.join("flake.nix"), "{ }").unwrap();
    std::fs::set_permissions(
        dir.join("flake.nix"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_ne!(base, hash_flake_dir(dir).unwrap());

    std::fs::set_permissions(
        dir.join("flake.nix"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::os::unix::fs::symlink("flake.nix", dir.join("link")).unwrap();
    assert_ne!(base, hash_flake_dir(dir).unwrap());
    assert_eq!(base, {
        std::fs::remove_file(dir.join("link")).unwrap();
        hash_flake_dir(dir).unwrap()
    });
}
