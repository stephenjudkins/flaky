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

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::image_fs::ImageFile;
use crate::nar::{DirNar, HashReader};
use crate::orchestrator::{BuildOpts, image_path_for};
use crate::vm::{BlkDev, Vm, VmSpec};

const NIX_CLOSURE_DIR: &str = "guest/nix-closure";
const NIX_CLOSURE_IMAGE: &str = "nix-closure.erofs";
const EXTRAS_DEVICE_SIZE: u64 = 8 << 30;

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
struct Locked {
    r#type: String,
    owner: String,
    repo: String,
    rev: String,
    nar_hash: String,
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
            bail!("input {name} has nested inputs (github-only v1)");
        }
        let locked = node
            .locked
            .as_ref()
            .with_context(|| format!("input {name} is not locked"))?;
        if locked.r#type != "github" {
            bail!("input {name} has unsupported type {}", locked.r#type);
        }
        out.push((name.clone(), Locked::clone(locked)));
    }
    Ok(out)
}

fn expected_nar_hash(nar_hash: &str) -> anyhow::Result<[u8; 32]> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(nar_hash.strip_prefix("sha256-").unwrap_or(nar_hash))
        .context("decoding narHash")?;
    raw.try_into().map_err(|_| anyhow!("narHash is not sha256"))
}

async fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    use futures::TryStreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    let resp = reqwest::get(url).await?.error_for_status()?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp
        .bytes_stream()
        .map_err(|e| anyhow::anyhow!("download: {e}"));
    while let Some(chunk) = stream.try_next().await? {
        file.write_all(chunk.as_ref()).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Fetches the github tarball, verifies its NAR hash against the lock, and
/// packs it as a hash-addressed erofs image. Streams the tarball directly
/// into the image; falls back to unpacking to disk when the tarball has an
/// unexpected shape.
async fn fetch_input_image(
    locked: &Locked,
    store_path: &str,
    dest: &Path,
    tmp_dir: &Path,
) -> anyhow::Result<()> {
    let url = format!(
        "https://github.com/{}/{}/archive/{}.tar.gz",
        locked.owner, locked.repo, locked.rev
    );
    eprintln!("flake: fetching {url}");
    let base = nix_drv::basename(store_path);
    let volume = apis::volume_id(store_path);
    let mut tmp_img = dest.as_os_str().to_owned();
    tmp_img.push(".download");
    let tmp_img = PathBuf::from(tmp_img);
    let top = format!("{}-{}", locked.repo, locked.rev);
    let result = async {
        let writer = match stream_fetch(&url, &top, base, &volume, &tmp_img).await {
            Ok(w) => w,
            Err(reason) => {
                eprintln!("flake: streaming fetch fell back to disk: {reason}");
                fetch_via_disk(&url, &top, tmp_dir, base, &volume, &tmp_img).await?
            }
        };
        let (file, size) = nar_to_erofs::finish_image(writer).await?;
        file.set_len(size).await?;
        file.sync_all().await?;
        drop(file);
        let digest = crate::tarball::image_nar_digest(&tmp_img)
            .await
            .map_err(anyhow::Error::msg)?;
        let expected = expected_nar_hash(&locked.nar_hash)?;
        anyhow::ensure!(
            digest == expected,
            "narHash mismatch for input {}/{}: expected {}, got {}",
            locked.owner,
            locked.repo,
            locked.nar_hash,
            nix_drv::nix_base32_encode(&digest),
        );
        std::fs::rename(&tmp_img, dest)?;
        println!(
            "flake: fetched {}/{} (image {} bytes)",
            locked.owner, locked.repo, size
        );
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_img);
    }
    result
}

async fn stream_fetch(
    url: &str,
    top: &str,
    base: &str,
    volume: &str,
    tmp_img: &Path,
) -> Result<crate::tarball::ImageWriter, String> {
    use futures::TryStreamExt as _;
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("download: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("download: {e}"))?;
    let stream = resp
        .bytes_stream()
        .map_err(|e| std::io::Error::other(format!("download: {e}")));
    let tar = async_compression::tokio::bufread::GzipDecoder::new(tokio::io::BufReader::new(
        tokio_util::io::StreamReader::new(stream),
    ));
    crate::tarball::tar_to_image(tar, top, base, volume, tmp_img).await
}

async fn fetch_via_disk(
    url: &str,
    top: &str,
    tmp_dir: &Path,
    base: &str,
    volume: &str,
    tmp_img: &Path,
) -> anyhow::Result<crate::tarball::ImageWriter> {
    let tgz = tmp_dir.join("input.tar.gz");
    download(url, &tgz).await?;

    let unpack = tmp_dir.join("unpacked");
    std::fs::create_dir_all(&unpack)?;
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(&tgz)?);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(&unpack)?;
    let tree = unpack.join(top);
    anyhow::ensure!(tree.is_dir(), "tarball has unexpected layout");

    let file = tokio::fs::File::create(tmp_img).await?;
    let mut writer = nar_to_erofs::image_writer(file, volume).await?;
    let mut decoder = nar_to_erofs::NarDecoder::new(HashReader::new(DirNar::new(&tree)));
    nar_to_erofs::write_nar(&mut decoder, &mut writer, Some(base)).await?;
    let _ = std::fs::remove_file(&tgz);
    let _ = std::fs::remove_dir_all(&unpack);
    Ok(writer)
}

fn prep_scratch(path: &Path, size: u64) -> anyhow::Result<()> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.set_len(size)?;
    Ok(())
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
        if produced.contains(p) || image_path_for(cache_dir, p).exists() {
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
    let evaluated = eval(&dir, std::slice::from_ref(&flake_ref.attr), &cache_dir).await?;
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
/// Uncached attrs are evaluated by nix inside one guest VM; source paths
/// nix creates during eval across all attrs are packed into one shared
/// extras image.
pub(crate) async fn eval(
    dir: &Path,
    attrs: &[String],
    cache_dir: &Path,
) -> anyhow::Result<Evaluated> {
    let lock_text = std::fs::read_to_string(dir.join("flake.lock"))
        .context("reading flake.lock (is this a flake?)")?;
    let lock: Lock = serde_json::from_str(&lock_text).context("parsing flake.lock")?;
    let inputs = lock_inputs(&lock)?;

    for d in ["erofs", "tmp", "eval"] {
        std::fs::create_dir_all(cache_dir.join(d))?;
    }

    let nix_root = std::fs::read_to_string(Path::new(NIX_CLOSURE_DIR).join("root"))?
        .trim()
        .to_string();

    let mut drv_jsons = BTreeMap::new();
    let mut extra_images = BTreeMap::new();
    let mut uncached = Vec::new();
    for attr in attrs {
        let key = eval_cache_key(attr, &inputs, dir, &nix_root)?;
        let cache_file = cache_dir.join("eval").join(format!("{key}.json"));
        let cached = std::fs::read_to_string(&cache_file)
            .ok()
            .and_then(|t| serde_json::from_str::<EvalCacheEntry>(&t).ok())
            .and_then(|entry| {
                extras_from_eval(&entry, cache_dir)
                    .ok()
                    .flatten()
                    .map(|extras| (entry.drv.clone(), extras))
            });
        match cached {
            Some((text, extras)) => {
                println!("flake: eval cache hit ({attr})");
                let _ = std::fs::write(cache_dir.join("last-eval.json"), &text);
                drv_jsons.insert(attr.clone(), text);
                extra_images.extend(extras);
            }
            None => uncached.push(attr.clone()),
        }
    }

    if !uncached.is_empty() {
        let (texts, extras) = eval_in_vm(dir, &inputs, &uncached, &nix_root, cache_dir).await?;
        drv_jsons.extend(texts);
        extra_images.extend(extras);
    }

    Ok(Evaluated {
        drv_jsons,
        extra_images,
    })
}

/// Closure source paths of a fresh eval that neither build outputs nor
/// cached per-path images cover: these must come from the extras image.
fn missing_paths(drv_json: &str, cache_dir: &Path) -> anyhow::Result<Vec<String>> {
    let drvs = nix_drv::parse(drv_json).context("parsing derivation show output")?;
    let root = nix_drv::find_root(&drvs)
        .ok_or_else(|| anyhow!("no unique root derivation in eval output"))?;
    let closure = nix_drv::closure(&drvs, &root);
    let produced: std::collections::BTreeSet<&String> = closure
        .drvs
        .iter()
        .flat_map(|d| drvs[d].outputs.values().map(|o| &o.path))
        .collect();
    Ok(closure
        .store_paths
        .into_iter()
        .filter(|p| !produced.contains(p) && !image_path_for(cache_dir, p).exists())
        .collect())
}

async fn eval_in_vm(
    dir: &Path,
    inputs: &[(String, Locked)],
    attrs: &[String],
    nix_root: &str,
    cache_dir: &Path,
) -> anyhow::Result<(BTreeMap<String, String>, BTreeMap<String, PathBuf>)> {
    let mut images = vec![ImageFile {
        name: NIX_CLOSURE_IMAGE.to_string(),
        path: PathBuf::from(NIX_CLOSURE_DIR).join(NIX_CLOSURE_IMAGE),
    }];
    let mut specs = Vec::new();
    for (name, locked) in inputs {
        let store_path = nix_drv::source_store_path(&locked.nar_hash);
        let dest = image_path_for(cache_dir, &store_path);
        if dest.exists() {
            eprintln!("flake: input {name} already cached");
        } else {
            let tmp = cache_dir
                .join("tmp")
                .join(format!("input-{}", apis::store_hash(&store_path)));
            fetch_input_image(locked, &store_path, &dest, &tmp).await?;
        }
        images.push(ImageFile {
            name: apis::image_name(&store_path),
            path: dest,
        });
        specs.push(apis::FlakeInputSpec {
            name: name.clone(),
            image: apis::image_name(&store_path),
            store_path,
        });
    }

    let scratch = cache_dir.join("tmp").join("flake-extras.img");
    prep_scratch(&scratch, EXTRAS_DEVICE_SIZE)?;

    let mut spec = VmSpec::guest(
        6144,
        2,
        images,
        vec![BlkDev {
            path: scratch.clone(),
            readonly: false,
        }],
    );
    spec.src_dir = Some(dir.to_path_buf());
    let vm = Vm::boot(spec).context("booting eval vm")?;

    let req = apis::FlakeEvalRequest {
        nix_image: NIX_CLOSURE_IMAGE.to_string(),
        nix_root: nix_root.to_string(),
        attrs: attrs.to_vec(),
        inputs: specs,
    };

    let rpc_cache_dir = cache_dir.to_path_buf();
    let rpc_attrs = attrs.to_vec();
    let outcome = vm
        .guest_rpc(move |c| async move {
            let text = c
                .flake_eval(crate::rpc::rpc_context(), req)
                .await
                .map_err(|e| anyhow::anyhow!("flake_eval rpc: {e}"))?
                .map_err(anyhow::Error::msg)?;
            let v: serde_json::Value =
                serde_json::from_str(&text).context("parsing flake_eval response")?;
            let roots = v["roots"].as_object().context("flake_eval roots")?;
            let show = v
                .get("show")
                .cloned()
                .context("flake_eval derivation output")?;
            let _ = std::fs::write(
                rpc_cache_dir.join("last-eval.json"),
                serde_json::to_string_pretty(&show)?,
            );
            let show_text = serde_json::to_string(&show)?;
            let drvs = nix_drv::parse(&show_text).context("parsing derivation show output")?;
            // v4 show output nests the map under "derivations"
            let inner = match show.get("derivations") {
                Some(d) => d,
                None => &show,
            };
            let inner = inner.as_object().context("derivation show output")?;
            let mut per_attr = BTreeMap::new();
            for attr in &rpc_attrs {
                let root = match roots.get(attr).and_then(|r| r.as_str()) {
                    Some(r) => r.to_string(),
                    None => nix_drv::find_root(&drvs)
                        .ok_or_else(|| anyhow!("no unique root derivation in eval output"))?,
                };
                let closure = nix_drv::closure(&drvs, &root);
                let mut filtered = serde_json::Map::new();
                for d in &closure.drvs {
                    let entry = inner
                        .get(d)
                        .cloned()
                        .ok_or_else(|| anyhow!("eval output missing drv {d}"))?;
                    filtered.insert(d.clone(), entry);
                }
                let text = match show.get("version") {
                    Some(version) => {
                        let mut obj = serde_json::Map::new();
                        obj.insert("version".to_string(), version.clone());
                        obj.insert(
                            "derivations".to_string(),
                            serde_json::Value::Object(filtered),
                        );
                        serde_json::Value::Object(obj).to_string()
                    }
                    None => serde_json::Value::Object(filtered).to_string(),
                };
                let missing = missing_paths(&text, &rpc_cache_dir)?;
                per_attr.insert(attr.clone(), (text, missing));
            }
            let union: std::collections::BTreeSet<String> = per_attr
                .values()
                .flat_map(|(_, m)| m.iter().cloned())
                .collect();
            let mut extra_images = BTreeMap::new();
            let mut extras_name = None;
            if union.is_empty() {
                let _ = std::fs::remove_file(&scratch);
            } else {
                println!(
                    "flake: packing {} eval-created source paths from guest",
                    union.len()
                );
                let size = c
                    .pack_store_paths(
                        crate::rpc::rpc_context(),
                        apis::PackPathsRequest {
                            paths: union.iter().cloned().collect(),
                            device: "/dev/vda".to_string(),
                        },
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("pack_store_paths rpc: {e}"))?
                    .map_err(anyhow::Error::msg)?;
                let f = std::fs::OpenOptions::new().write(true).open(&scratch)?;
                f.set_len(size)?;
                let mut h = Sha256::new();
                for p in &union {
                    h.update(p.as_bytes());
                    h.update(b"\0");
                }
                let name = format!("flake-extras-{}.erofs", hex(h.finalize().into()));
                let dest = rpc_cache_dir.join("erofs").join(&name);
                std::fs::rename(&scratch, &dest)?;
                for p in &union {
                    extra_images.insert(p.clone(), dest.clone());
                }
                extras_name = Some(name);
            }
            Ok((per_attr, extra_images, extras_name))
        })
        .await;
    vm.reap(std::time::Duration::from_secs(120)).await;
    let (per_attr, extra_images, extras_name) = outcome?;

    for (attr, (text, missing)) in &per_attr {
        let key = eval_cache_key(attr, inputs, dir, nix_root)?;
        let entry = EvalCacheEntry {
            drv: text.clone(),
            packed: missing.clone(),
            extras_image: match &extras_name {
                Some(name) if !missing.is_empty() => Some(name.clone()),
                _ => None,
            },
        };
        let cache_file = cache_dir.join("eval").join(format!("{key}.json"));
        std::fs::write(&cache_file, serde_json::to_string(&entry)?)?;
    }

    Ok((
        per_attr
            .into_iter()
            .map(|(attr, (text, _))| (attr, text))
            .collect(),
        extra_images,
    ))
}

#[cfg(test)]
mod tests;
