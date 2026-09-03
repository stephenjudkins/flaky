use super::scan_store_paths;

#[test]
fn scans_store_paths_from_env_text() {
    let text = "builder=/nix/store/ki4g5pbmqcs07mwrxmvik6dpnzyx3z4c-bootstrap-tools/bin/bash \
                PATH=/nix/store/aaa1111aaa1111aaa1111aaa1111aa11-x/bin:/nix/store/bbb2222bbb2222bbb2222bbb2222bb22-y/bin \
                flags='--with-pic' json=\"{\\\"stdenv\\\": \\\"/nix/store/ccc3333ccc3333ccc3333ccc3333cc33-z\\\"}\"";
    let paths = scan_store_paths(text);
    assert_eq!(
        paths,
        vec![
            "/nix/store/ki4g5pbmqcs07mwrxmvik6dpnzyx3z4c-bootstrap-tools",
            "/nix/store/aaa1111aaa1111aaa1111aaa1111aa11-x",
            "/nix/store/bbb2222bbb2222bbb2222bbb2222bb22-y",
            "/nix/store/ccc3333ccc3333ccc3333ccc3333cc33-z",
        ]
    );
}

#[test]
fn scan_rejects_short_and_terminated() {
    assert!(scan_store_paths("no paths here /nix/store").is_empty());
    // name chars only after the prefix; punctuation terminates
    let v = scan_store_paths("x/nix/store/abc123/bin/sh foo");
    assert_eq!(v, vec!["/nix/store/abc123"]);
}
