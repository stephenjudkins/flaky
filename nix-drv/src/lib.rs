//! Parsing of `nix derivation show -r` JSON, closure computation, and
//! structured-attrs (`.attrs.sh`) generation shared by host and guest.

pub mod attrs;

pub use attrs::{generate_attrs_sh, shell_quote, structured_env};

use std::collections::{BTreeMap, BTreeSet, HashSet};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structuredAttrs: Option<serde_json::Value>,
}

pub type Derivations = BTreeMap<String, Derivation>;

pub fn parse(s: &str) -> Result<Derivations, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(s)?;
    if v.get("derivations").is_some() {
        parse_v4(v)
    } else {
        serde_json::from_value(v)
    }
}

#[derive(Deserialize)]
struct V4Doc {
    derivations: BTreeMap<String, V4Drv>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct V4Drv {
    name: String,
    outputs: BTreeMap<String, V4Output>,
    #[serde(default)]
    inputs: V4Inputs,
    system: String,
    builder: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    #[serde(default)]
    structured_attrs: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct V4Output {
    #[serde(default)]
    path: String,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    method: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct V4Inputs {
    #[serde(default)]
    drvs: BTreeMap<String, V4InputDrv>,
    #[serde(default)]
    srcs: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct V4InputDrv {
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    dynamic_outputs: serde_json::Value,
}

/// v4 names store paths relative to /nix/store.
fn abs_store_path(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/nix/store/{p}")
    }
}

fn parse_v4(v: serde_json::Value) -> Result<Derivations, serde_json::Error> {
    let doc: V4Doc = serde_json::from_value(v)?;
    let mut out = Derivations::new();
    for (name, d) in doc.derivations {
        let env_ref = &d.env;
        let drv = Derivation {
            args: d.args,
            builder: d.builder,
            env: d.env.clone(),
            inputDrvs: d
                .inputs
                .drvs
                .into_iter()
                .map(|(p, i)| {
                    (
                        abs_store_path(&p),
                        InputDrv {
                            outputs: i.outputs,
                            dynamicOutputs: i.dynamic_outputs,
                        },
                    )
                })
                .collect(),
            inputSrcs: d.inputs.srcs.iter().map(|p| abs_store_path(p)).collect(),
            name: d.name,
            outputs: d
                .outputs
                .into_iter()
                .map(|(n, o)| {
                    // v4 show omits the path for fixed-output derivations;
                    // env always carries it
                    let path = if o.path.is_empty() {
                        env_ref.get(&n).cloned().unwrap_or_default()
                    } else {
                        abs_store_path(&o.path)
                    };
                    (
                        n,
                        Output {
                            path,
                            hash: o.hash,
                            hashAlgo: o.method,
                        },
                    )
                })
                .collect(),
            system: d.system,
            structuredAttrs: d.structured_attrs,
        };
        out.insert(abs_store_path(&name), drv);
    }
    Ok(out)
}

/// Looks up a derivation option in `env` or, for structured attrs, in the
/// `structuredAttrs` JSON (strings as-is, bools as "1"/"", lists joined
/// with spaces).
pub fn env_value(drv: &Derivation, key: &str) -> Option<String> {
    if let Some(v) = drv.env.get(key) {
        return Some(v.clone());
    }
    let sa = drv.structuredAttrs.as_ref()?;
    match sa.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(if *b { "1".to_string() } else { String::new() }),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .map(|v| v.as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        serde_json::Value::Null => None,
        _ => None,
    }
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

/// The store path a `source:sha256-<base64>` content-addressed tree lands
/// at (what `nix flake archive` / the path fetcher compute from a lock
/// node's narHash).
pub fn source_store_path(nar_hash: &str) -> String {
    let raw: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(nar_hash.strip_prefix("sha256-").unwrap_or(nar_hash))
        .expect("decoding narHash")
        .try_into()
        .expect("narHash is sha256");
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let preimage = format!("source:sha256:{hex}:/nix/store:source");
    let digest = Sha256::digest(preimage.as_bytes());
    let folded: [u8; 20] = std::array::from_fn(|i| {
        let mut b = digest[i];
        if i + 20 < 32 {
            b ^= digest[i + 20];
        }
        b
    });
    format!("/nix/store/{}-source", nix_base32_encode(&folded))
}

#[cfg(test)]
mod tests;
