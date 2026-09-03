use super::*;
use serde_json::json;

fn sample() -> Derivations {
    let v = json!({
        "/nix/store/aaa-hello.drv": {
            "args": ["-e", "/nix/store/ccc-stdenv.sh"],
            "builder": "/nix/store/ddd-bash/bin/bash",
            "env": {"__json": "{\"name\":\"hello\"}", "out": "/nix/store/eee-hello"},
            "inputDrvs": {"/nix/store/bbb-fetch.drv": {"outputs": ["out"]}},
            "inputSrcs": ["/nix/store/fff-src"],
            "name": "hello",
            "outputs": {"out": {"path": "/nix/store/eee-hello"}},
            "system": "aarch64-linux"
        },
        "/nix/store/bbb-fetch.drv": {
            "args": [],
            "builder": "builtin:fetchurl",
            "env": {"url": "https://example.com/tar.gz", "out": "/nix/store/ggg-tar.gz"},
            "inputDrvs": {},
            "inputSrcs": [],
            "name": "tar.gz",
            "outputs": {"out": {"path": "/nix/store/ggg-tar.gz",
                                 "hash": "641cf30961525cbe3b340cc883436c8854e9f5032f459f444de4782b621e6572",
                                 "hashAlgo": "sha256"}},
            "system": "builtin"
        }
    });
    parse(&v.to_string()).unwrap()
}

#[test]
fn parses_sample() {
    let drvs = sample();
    assert_eq!(drvs.len(), 2);
    let hello = &drvs["/nix/store/aaa-hello.drv"];
    assert_eq!(hello.system, "aarch64-linux");
    assert_eq!(hello.env.get("__json").unwrap(), "{\"name\":\"hello\"}");
    let fetch = &drvs["/nix/store/bbb-fetch.drv"];
    assert_eq!(
        fetch.outputs["out"].hash.as_deref(),
        Some("641cf30961525cbe3b340cc883436c8854e9f5032f459f444de4782b621e6572")
    );
}

#[test]
fn finds_root() {
    let drvs = sample();
    assert_eq!(
        find_root(&drvs).as_deref(),
        Some("/nix/store/aaa-hello.drv")
    );
}

#[test]
fn computes_closure() {
    let drvs = sample();
    let c = closure(&drvs, "/nix/store/aaa-hello.drv");
    assert_eq!(
        c.drvs,
        ["/nix/store/aaa-hello.drv", "/nix/store/bbb-fetch.drv"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert_eq!(
        c.store_paths,
        [
            "/nix/store/eee-hello",
            "/nix/store/fff-src",
            "/nix/store/ggg-tar.gz"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
}

#[test]
fn nix_base32_matches_nix() {
    // generated with `nix hash to-base32 --type sha256 <hex>`
    assert_eq!(
        nix_base32_encode(&[0; 32]),
        "0000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(
        nix_base32_encode(&[0xff; 32]),
        "1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    );
    assert_eq!(
        nix_base32_encode(&hex(
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"
        )),
        "080z3qfiq6qs34c1f5hm2h9i448h1w70s30b184hh1q60l2060h1"
    );
    assert_eq!(
        nix_base32_encode(&hex(
            "641cf30961525cbe3b340cc883436c8854e9f5032f459f444de4782b621e6572"
        )),
        "0wk53ri2ny749m29yi9g0gsyjm48di1q7j0c6hxvwp2jc44z6734"
    );
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
