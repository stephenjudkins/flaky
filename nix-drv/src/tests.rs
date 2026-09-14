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

#[test]
fn nix_base32_decode_inverts_encode() {
    for bytes in [
        vec![0u8; 32],
        vec![0xff; 32],
        hex("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"),
        hex("641cf30961525cbe3b340cc883436c8854e9f5032f459f444de4782b621e6572"),
    ] {
        let s = nix_base32_encode(&bytes);
        assert_eq!(nix_base32_decode(&s).as_deref(), Some(&bytes[..]));
    }
}

#[test]
fn nix_base32_decode_matches_nix() {
    // generated with `nix hash to-base32 --type sha256 <hex>`
    assert_eq!(
        nix_base32_decode("0wk53ri2ny749m29yi9g0gsyjm48di1q7j0c6hxvwp2jc44z6734").as_deref(),
        Some(&hex("641cf30961525cbe3b340cc883436c8854e9f5032f459f444de4782b621e6572")[..])
    );
    assert_eq!(
        nix_base32_decode("1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").as_deref(),
        Some(&[0xff; 32][..])
    );
    assert!(nix_base32_decode("0").is_none());
    assert!(nix_base32_decode("e").is_none()); // not in alphabet
    assert!(nix_base32_decode(&"0".repeat(51)).is_none());
    assert_eq!(
        nix_base32_decode(&"0".repeat(52)).as_deref(),
        Some(&[0u8; 32][..])
    );
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn source_store_path_matches_nix() {
    // verified against `nix flake archive` for the samples flake.lock inputs
    assert_eq!(
        source_store_path("sha256-dt4WdcvsA8/RCe+VZZwqU0X+XMM3wBbGCWA0/sFWzGo="),
        "/nix/store/d6mryll7gbj6hbczvyrvnflcyxxq11zn-source"
    );
    assert_eq!(
        source_store_path("sha256-Mziif8w1I2tc2rL8g5ov0PrKzmVZaEvAEAZjsyyvdTk="),
        "/nix/store/1c9r7jikgbcvh746pwvpp7652vr3zwsi-source"
    );
}

#[test]
fn parses_v4_derivation_show() {
    // captured from `nix --offline derivation show -r` (nix 2.34, v4 format)
    let text = r#"{
  "derivations": {
    "fxi28ig53ga2h6mavj21jfs6kkdsqzry-flaky-sample-write-file.drv": {
      "args": ["-e", "/nix/store/l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh"],
      "builder": "/nix/store/3kaakrwd1jia637aa3cpj3lkgfxwg3k3-bash-5.3p15/bin/bash",
      "env": {"__structuredAttrs": "", "buildCommand": "mkdir $out", "out": "/nix/store/c72cyy4yl1ay0vkna42k79rcijnamqng-flaky-sample-write-file", "passAsFile": "buildCommand"},
      "inputs": {
        "drvs": {"gijz9fmbq78n1877ycpyxdks6d141w90-bootstrap-stage4-stdenv-linux.drv": {"dynamicOutputs": {}, "outputs": ["out"]}},
        "srcs": ["l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh", "shkw4qm9qcw5sc5n1k5jznc83ny02r39-default-builder.sh"]
      },
      "name": "flaky-sample-write-file",
      "outputs": {"out": {"path": "c72cyy4yl1ay0vkna42k79rcijnamqng-flaky-sample-write-file"}},
      "system": "aarch64-linux",
      "version": 4
    }
  },
  "version": 4
}"#;
    let drvs = parse(text).unwrap();
    let key = "/nix/store/fxi28ig53ga2h6mavj21jfs6kkdsqzry-flaky-sample-write-file.drv";
    assert_eq!(drvs.len(), 1);
    let d = &drvs[key];
    assert_eq!(d.name, "flaky-sample-write-file");
    assert_eq!(
        d.outputs["out"].path,
        "/nix/store/c72cyy4yl1ay0vkna42k79rcijnamqng-flaky-sample-write-file"
    );
    assert_eq!(
        d.inputSrcs,
        vec![
            "/nix/store/l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh".to_string(),
            "/nix/store/shkw4qm9qcw5sc5n1k5jznc83ny02r39-default-builder.sh".to_string()
        ]
    );
    assert_eq!(
        d.inputDrvs.keys().next().map(String::as_str),
        Some("/nix/store/gijz9fmbq78n1877ycpyxdks6d141w90-bootstrap-stage4-stdenv-linux.drv")
    );
}

#[test]
fn parses_v4_structured_attrs_fetchurl() {
    let text = r#"{
  "derivations": {
    "0p57d8lpv8i125pn7xp7h1jhjpx4xf7c-Compress-Raw-Zlib-2.222.tar.gz.drv": {
      "args": ["builtin:fetchurl"],
      "builder": "builtin:fetchurl",
      "env": {"out": "/nix/store/gmrhyy2xvh667zsvjgiv00kj7y6ca8nc-lzip-1.26.tar.gz"},
      "inputs": {"drvs": {}, "srcs": []},
      "name": "lzip-1.26.tar.gz",
      "outputs": {"out": {"hash": "sha256-ZBzzCWFSXL47NAzIg0NsiFTp9QMvRZ9ETeR4K2IeZXI=", "method": "flat", "path": "gmrhyy2xvh667zsvjgiv00kj7y6ca8nc-lzip-1.26.tar.gz"}},
      "structuredAttrs": {"executable": false, "outputHash": "sha256-ZBzzCWFSXL47NAzIg0NsiFTp9QMvRZ9ETeR4K2IeZXI=", "outputHashAlgo": "sha256", "outputHashMode": "flat", "urls": ["https://example.com/lzip-1.26.tar.gz"]},
      "system": "builtin",
      "version": 4
    }
  },
  "version": 4
}"#;
    let drvs = parse(text).unwrap();
    let d = &drvs["/nix/store/0p57d8lpv8i125pn7xp7h1jhjpx4xf7c-Compress-Raw-Zlib-2.222.tar.gz.drv"];
    assert_eq!(
        env_value(d, "outputHash").as_deref(),
        Some("sha256-ZBzzCWFSXL47NAzIg0NsiFTp9QMvRZ9ETeR4K2IeZXI=")
    );
    assert_eq!(
        env_value(d, "urls").as_deref(),
        Some("https://example.com/lzip-1.26.tar.gz")
    );
    assert_eq!(env_value(d, "executable").as_deref(), Some(""));
    assert_eq!(d.builder, "builtin:fetchurl");
}
