//! `flaky flake <dir>#<attr>`: evaluate a flake with nix inside a guest VM
//! (no host nix), then build the resulting derivation closure with the
//! orchestrator.
//!
//! Flake.lock inputs become hash-addressed erofs images under
//! `.cache/erofs/`: the store path of a `github` node follows from its
//! narHash, so cache hits need no network. The eval VM gets the nix
//! closure image, the input images, and the host flake directory via a
//! read-only `flaky-src` virtio-fs mount; nix runs `--offline` with every
//! root input overridden to its erofs-mounted tree. Source paths nix
//! creates during eval (the flake root copy, script/text sources) are
//! packed by the guest into one extra image and bound into build VMs.
//!
//! The VM work runs as tasks (see `crate::tasks::eval`); this module owns
//! lock parsing and the on-disk eval cache.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::orchestrator::{BuildOpts, Orchestrator};
use crate::tasks::{EvalFlake, EvalOutcome};

pub(crate) const NIX_CLOSURE_DIR: &str = "guest/nix-closure";
pub(crate) const NIX_CLOSURE_IMAGE: &str = "nix-closure.erofs";

#[derive(Debug, Deserialize)]
struct Lock {
    nodes: BTreeMap<String, LockNode>,
    root: String,
}

#[derive(Debug, Default, Deserialize)]
struct LockNode {
    #[serde(default)]
    locked: Option<Locked>,
    #[serde(default)]
    inputs: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Locked {
    pub(crate) r#type: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) rev: String,
    pub(crate) nar_hash: String,
}

pub struct FlakeRef {
    pub dir: PathBuf,
    pub attr: String,
}

pub fn parse_flake_ref(s: &str) -> anyhow::Result<FlakeRef> {
    let (dir, attr) = match s.split_once('#') {
        Some((d, a)) => (d, a),
        None => (s, "default"),
    };
    let dir = if dir.is_empty() { "." } else { dir };
    Ok(FlakeRef {
        dir: PathBuf::from(dir),
        attr: attr.to_string(),
    })
}

/// Root flake inputs: (input name, github node). Errors on non-github or
/// nested inputs (v1: github-only).
fn lock_inputs(lock: &Lock) -> anyhow::Result<Vec<(String, Locked)>> {
    let root = lock
        .nodes
        .get(&lock.root)
        .ok_or_else(|| anyhow!("lock has no root node"))?;
    let inputs = root.inputs.as_ref().context("flake has no inputs")?;
    let mut out = Vec::new();
    for (name, node_id) in inputs {
        let node_id = node_id
            .as_str()
            .ok_or_else(|| anyhow!("input {name} uses a follows-style reference"))?;
        let node = lock
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow!("lock references missing node {node_id}"))?;
        if node.inputs.is_some() {
            anyhow::bail!("input {name} has nested inputs (github-only v1)");
        }
        let locked = node
            .locked
            .as_ref()
            .with_context(|| format!("input {name} is not locked"))?;
        if locked.r#type != "github" {
            anyhow::bail!("input {name} has unsupported type {}", locked.r#type);
        }
        out.push((name.clone(), Locked::clone(locked)));
    }
    Ok(out)
}

pub(crate) fn expected_nar_hash(nar_hash: &str) -> anyhow::Result<[u8; 32]> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(nar_hash.strip_prefix("sha256-").unwrap_or(nar_hash))
        .context("decoding narHash")?;
    raw.try_into().map_err(|_| anyhow!("narHash is not sha256"))
}

struct HashWriter<'a>(&'a mut Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hex(d: [u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(path)?;
    std::io::copy(&mut f, &mut HashWriter(&mut h))?;
    Ok(hex(h.finalize().into()))
}

/// Content hash of the flake's own source tree. Names cannot contain NUL,
/// so NUL-termination is unambiguous; the executable bit participates
/// because it affects the store paths nix assigns during eval.
fn hash_flake_dir(dir: &Path) -> anyhow::Result<String> {
    fn walk(h: &mut Sha256, dir: &Path, prefix: &[u8]) -> anyhow::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let rel = [prefix, e.file_name().as_encoded_bytes()].concat();
            let md = std::fs::symlink_metadata(e.path())?;
            let ft = md.file_type();
            if ft.is_symlink() {
                h.update(b"L");
                h.update(&rel);
                h.update(b"\0");
                let target = std::fs::read_link(e.path())?;
                h.update(target.as_os_str().as_encoded_bytes());
                h.update(b"\0");
            } else if ft.is_dir() {
                h.update(b"D");
                h.update(&rel);
                h.update(b"\0");
                let child_prefix = [rel.as_slice(), b"/"].concat();
                walk(h, &e.path(), &child_prefix)?;
            } else {
                h.update(b"F");
                h.update((md.mode() & 0o7777).to_le_bytes());
                h.update(&rel);
                h.update(b"\0");
                h.update(md.len().to_le_bytes());
                let mut f = std::fs::File::open(e.path())?;
                std::io::copy(&mut f, &mut HashWriter(h))?;
            }
        }
        Ok(())
    }
    let mut h = Sha256::new();
    walk(&mut h, dir, b"")?;
    Ok(hex(h.finalize().into()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EvalCacheEntry {
    drv: String,
    packed: Vec<String>,
    #[serde(default)]
    extras_image: Option<String>,
}

/// Eval output is determined by the attr, the locked inputs, the flake's
/// own source tree, and the guest nix (store path + closure image).
fn eval_cache_key(
    attr: &str,
    inputs: &[(String, Locked)],
    flake_dir: &Path,
    nix_root: &str,
) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    h.update(b"flaky-eval-v1\0");
    h.update(attr.as_bytes());
    h.update(b"\0");
    for (name, locked) in inputs {
        h.update(name.as_bytes());
        h.update(b"\0");
        h.update(locked.owner.as_bytes());
        h.update("/");
        h.update(locked.repo.as_bytes());
        h.update("@");
        h.update(locked.rev.as_bytes());
        h.update(b"\0");
        h.update(locked.nar_hash.as_bytes());
        h.update(b"\0");
    }
    h.update(hash_flake_dir(flake_dir)?.as_bytes());
    h.update(b"\0");
    h.update(nix_root.as_bytes());
    h.update(b"\0");
    h.update(hash_file(&Path::new(NIX_CLOSURE_DIR).join(NIX_CLOSURE_IMAGE))?.as_bytes());
    Ok(hex(h.finalize().into()))
}

/// Rebuilds the orchestrator inputs from a cached eval: every closure
/// source path lacking a per-path image must be covered by the stored
/// extras image, else the cache entry is unusable.
fn extras_from_eval(
    entry: &EvalCacheEntry,
    cache_dir: &Path,
) -> anyhow::Result<Option<BTreeMap<String, PathBuf>>> {
    let drvs = nix_drv::parse(&entry.drv)?;
    let root = nix_drv::find_root(&drvs).context("no unique root derivation in cached eval")?;
    let closure = nix_drv::closure(&drvs, &root);
    let produced: std::collections::BTreeSet<&String> = closure
        .drvs
        .iter()
        .flat_map(|d| drvs[d].outputs.values().map(|o| &o.path))
        .collect();
    let mut missing = Vec::new();
    for p in &closure.store_paths {
        if produced.contains(p) || crate::orchestrator::image_path_for(cache_dir, p).exists() {
            continue;
        }
        if !entry.packed.contains(p) {
            return Ok(None);
        }
        missing.push(p.clone());
    }
    let mut extra_images = BTreeMap::new();
    if !missing.is_empty() {
        let name = match &entry.extras_image {
            Some(name) => name.clone(),
            None => format!("flake-extras-{}.erofs", apis::store_hash(&root)),
        };
        let dest = cache_dir.join("erofs").join(&name);
        if !dest.exists() {
            return Ok(None);
        }
        for p in missing {
            extra_images.insert(p, dest.clone());
        }
    }
    Ok(Some(extra_images))
}

pub async fn run(
    flake_ref: FlakeRef,
    cache_url: String,
    cache_dir: PathBuf,
) -> anyhow::Result<String> {
    let dir = flake_ref
        .dir
        .canonicalize()
        .with_context(|| format!("resolving flake dir {}", flake_ref.dir.display()))?;
    let orch = Orchestrator::new(&cache_url, cache_dir.clone())?;
    let evaluated = eval(&orch, &dir, std::slice::from_ref(&flake_ref.attr)).await?;
    let drv_json = evaluated
        .drv_jsons
        .into_values()
        .next()
        .context("eval returned no derivation")?;
    crate::orchestrator::run(BuildOpts {
        drv_json,
        cache_url,
        cache_dir,
        extra_images: evaluated.extra_images,
    })
    .await
}

pub(crate) struct Evaluated {
    pub(crate) drv_jsons: BTreeMap<String, String>,
    pub(crate) extra_images: BTreeMap<String, PathBuf>,
}

/// Evaluates `dir#attr` for each attr, using the eval cache where valid.
/// Uncached attrs are evaluated by one EvalFlake task (nix in a guest VM);
/// source paths nix creates during eval across all attrs are packed into
/// one shared extras image.
pub(crate) async fn eval(
    orch: &Orchestrator,
    dir: &Path,
    attrs: &[String],
) -> anyhow::Result<Evaluated> {
    let ctx = orch.context();
    let lock_text = std::fs::read_to_string(dir.join("flake.lock"))
        .context("reading flake.lock (is this a flake?)")?;
    let lock: Lock = serde_json::from_str(&lock_text).context("parsing flake.lock")?;
    let inputs = lock_inputs(&lock)?;

    std::fs::create_dir_all(ctx.cache_dir().join("eval"))?;
    let nix_root = std::fs::read_to_string(Path::new(NIX_CLOSURE_DIR).join("root"))?
        .trim()
        .to_string();

    let mut drv_jsons = BTreeMap::new();
    let mut extra_images = BTreeMap::new();
    let mut uncached = Vec::new();
    for attr in attrs {
        let key = eval_cache_key(attr, &inputs, dir, &nix_root)?;
        let cache_file = ctx.cache_dir().join("eval").join(format!("{key}.json"));
        let cached = std::fs::read_to_string(&cache_file)
            .ok()
            .and_then(|t| serde_json::from_str::<EvalCacheEntry>(&t).ok())
            .and_then(|entry| {
                extras_from_eval(&entry, ctx.cache_dir())
                    .ok()
                    .flatten()
                    .map(|extras| (entry.drv.clone(), extras))
            });
        match cached {
            Some((text, extras)) => {
                println!("flake: eval cache hit ({attr})");
                let _ = std::fs::write(ctx.cache_dir().join("last-eval.json"), &text);
                drv_jsons.insert(attr.clone(), text);
                extra_images.extend(extras);
            }
            None => uncached.push(attr.clone()),
        }
    }

    if !uncached.is_empty() {
        let outcome: EvalOutcome = ctx
            .spawn(EvalFlake {
                dir: dir.to_path_buf(),
                inputs: inputs.clone(),
                attrs: uncached.clone(),
                nix_root: nix_root.clone(),
            })
            .await?;
        drv_jsons.extend(outcome.drv_jsons);
        extra_images.extend(outcome.extra_images);
        for attr in &uncached {
            let missing = outcome.missing.get(attr).cloned().unwrap_or_default();
            let key = eval_cache_key(attr, &inputs, dir, &nix_root)?;
            let entry = EvalCacheEntry {
                drv: drv_jsons[attr].clone(),
                packed: missing.clone(),
                extras_image: match &outcome.extras_image {
                    Some(name) if !missing.is_empty() => Some(name.clone()),
                    _ => None,
                },
            };
            let cache_file = ctx.cache_dir().join("eval").join(format!("{key}.json"));
            std::fs::write(&cache_file, serde_json::to_string(&entry)?)?;
        }
    }

    Ok(Evaluated {
        drv_jsons,
        extra_images,
    })
}

#[cfg(test)]
mod tests;
