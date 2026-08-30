//! Parsing of `nix derivation show -r` JSON, closure computation, and
//! structured-attrs (`.attrs.sh`) generation shared by host and guest.

pub mod attrs;

pub use attrs::{generate_attrs_sh, shell_quote, structured_env};

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

/// The 32-character nix base32 alphabet (excludes e, o, t, u).
pub const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";

/// Encode a digest in nix base32 (e.g. sha256 -> 52 chars).
pub fn nix_base32_encode(bytes: &[u8]) -> String {
    let num_chars = (bytes.len() * 8 - 1) / 5 + 1;
    let mut s = String::with_capacity(num_chars);
    for i in (0..num_chars).rev() {
        let b = i * 5;
        let (k, j) = (b / 8, b % 8);
        let mut c = (bytes[k] >> j) as u32;
        if k + 1 < bytes.len() {
            c |= (bytes[k + 1] as u32) << (8 - j);
        }
        s.push(NIX_BASE32.as_bytes()[(c & 0x1f) as usize] as char);
    }
    s
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashAlgo: Option<String>,
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputDrv {
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub dynamicOutputs: serde_json::Value,
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Derivation {
    pub args: Vec<String>,
    pub builder: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub inputDrvs: BTreeMap<String, InputDrv>,
    #[serde(default)]
    pub inputSrcs: Vec<String>,
    pub name: String,
    pub outputs: BTreeMap<String, Output>,
    pub system: String,
}

pub type Derivations = BTreeMap<String, Derivation>;

pub fn parse(s: &str) -> Result<Derivations, serde_json::Error> {
    serde_json::from_str(s)
}

/// The derivation not referenced by any other (the root of a
/// `nix derivation show -r` file). Returns None if there is no unique root.
pub fn find_root(drvs: &Derivations) -> Option<String> {
    let referenced: HashSet<&str> = drvs
        .values()
        .flat_map(|d| d.inputDrvs.keys().map(String::as_str))
        .collect();
    let roots: Vec<&String> = drvs
        .keys()
        .filter(|k| !referenced.contains(k.as_str()))
        .collect();
    roots.first().map(|s| (*s).clone())
}

#[derive(Debug, Default, Clone)]
pub struct Closure {
    pub drvs: BTreeSet<String>,
    pub store_paths: BTreeSet<String>,
}

pub fn closure(drvs: &Derivations, root: &str) -> Closure {
    let mut out = Closure::default();
    let mut stack = vec![root.to_string()];
    while let Some(p) = stack.pop() {
        if !out.drvs.insert(p.clone()) {
            continue;
        }
        let Some(d) = drvs.get(&p) else {
            continue;
        };
        out.store_paths
            .extend(d.outputs.values().map(|o| o.path.clone()));
        out.store_paths.extend(d.inputSrcs.iter().cloned());
        stack.extend(d.inputDrvs.keys().cloned());
    }
    out
}

pub fn basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
}

pub fn hash_part(path: &str) -> &str {
    basename(path)
        .split_once('-')
        .map(|(h, _)| h)
        .unwrap_or(basename(path))
}

#[cfg(test)]
mod tests {
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
}
