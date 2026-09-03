use super::{FlakeRef, lock_inputs, parse_flake_ref};

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
