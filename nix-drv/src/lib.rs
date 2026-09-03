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
mod tests;
