use super::*;
use serde_json::json;

fn hello_like() -> Value {
    json!({
        "name": "hello-2.12.3",
        "version": "2.12.3",
        "strictDeps": false,
        "doCheck": true,
        "patches": [],
        "outputs": ["out"],
        "nativeBuildInputs": ["/nix/store/p1-pkg", "/nix/store/p2-pkg"],
        "outputChecks": {"out": {}},
        "builder": "/nix/store/dddd-bash/bin/bash",
        "stdenv": "/nix/store/std-stdenv",
        "src": "/nix/store/wj7-hello.tar.gz",
        "env": {
            "NIX_HARDENING_ENABLE": "fortify",
            "emptyBool": false
        },
        "system": "aarch64-linux"
    })
}

const OUTS: &[(&str, &str)] = &[("out", "/nix/store/eee-hello")];

#[test]
fn quotes() {
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
}

#[test]
fn generates_attrs_sh() {
    let sh = generate_attrs_sh(
        &hello_like(),
        &OUTS
            .iter()
            .map(|(n, p)| (n.to_string(), p.to_string()))
            .collect::<Vec<_>>(),
    );
    assert!(sh.contains("declare -A outputs=( ['out']='/nix/store/eee-hello' )"));
    assert!(sh.contains("export out='/nix/store/eee-hello'"));
    assert!(sh.contains("export name='hello-2.12.3'"));
    assert!(sh.contains("export doCheck='1'"));
    assert!(!sh.contains("strictDeps"), "false booleans must be unset");
    assert!(sh.contains("declare -a patches=(  )"));
    assert!(
        sh.contains("declare -a nativeBuildInputs=( '/nix/store/p1-pkg' '/nix/store/p2-pkg' )")
    );
    assert!(sh.contains("declare -A outputChecks=( ['out']='' )"));
    assert!(sh.contains("export stdenv='/nix/store/std-stdenv'"));
    assert!(sh.contains("export src='/nix/store/wj7-hello.tar.gz'"));
    assert!(sh.contains("export NIX_HARDENING_ENABLE='fortify'"));
    assert!(!sh.contains("emptyBool"));
    assert!(
        !sh.contains("declare -a outputs"),
        "outputs must stay an assoc array"
    );
}

#[test]
fn flattens_env() {
    let env = structured_env(&hello_like());
    let m: std::collections::BTreeMap<_, _> = env.into_iter().collect();
    assert_eq!(m["NIX_HARDENING_ENABLE"], "fortify");
    assert!(!m.contains_key("emptyBool"));
}
