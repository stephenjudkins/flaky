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
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;

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
/// packs it as a hash-addressed erofs image.
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
    std::fs::create_dir_all(tmp_dir)?;
    let tgz = tmp_dir.join("input.tar.gz");
    download(&url, &tgz).await?;

    let unpack = tmp_dir.join("unpacked");
    std::fs::create_dir_all(&unpack)?;
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(&tgz)?);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(&unpack)?;
    let tree = unpack.join(format!("{}-{}", locked.repo, locked.rev));
    anyhow::ensure!(tree.is_dir(), "tarball has unexpected layout");

    let base = nix_drv::basename(store_path);
    let volume = apis::volume_id(store_path);
    let tmp_img = tmp_dir.join("image.erofs");
    let file = tokio::fs::File::create(&tmp_img).await?;
    let mut writer = nar_to_erofs::image_writer(file, &volume).await?;
    let mut decoder = nar_to_erofs::NarDecoder::new(HashReader::new(DirNar::new(&tree)));
    nar_to_erofs::write_nar(&mut decoder, &mut writer, Some(base)).await?;
    let digest = decoder.into_inner().digest();
    let expected = expected_nar_hash(&locked.nar_hash)?;
    anyhow::ensure!(
        digest == expected,
        "narHash mismatch for input {}/{}: expected {}, got {}",
        locked.owner,
        locked.repo,
        locked.nar_hash,
        nix_drv::nix_base32_encode(&digest),
    );
    let (file, size) = nar_to_erofs::finish_image(writer).await?;
    file.set_len(size).await?;
    file.sync_all().await?;
    std::fs::rename(&tmp_img, dest)?;
    let _ = std::fs::remove_file(&tgz);
    let _ = std::fs::remove_dir_all(&unpack);
    println!(
        "flake: fetched {}/{} (image {} bytes)",
        locked.owner, locked.repo, size
    );
    Ok(())
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

pub async fn run(
    flake_ref: FlakeRef,
    cache_url: String,
    cache_dir: PathBuf,
) -> anyhow::Result<String> {
    let dir = flake_ref
        .dir
        .canonicalize()
        .with_context(|| format!("resolving flake dir {}", flake_ref.dir.display()))?;
    let lock_text = std::fs::read_to_string(dir.join("flake.lock"))
        .context("reading flake.lock (is this a flake?)")?;
    let lock: Lock = serde_json::from_str(&lock_text).context("parsing flake.lock")?;
    let inputs = lock_inputs(&lock)?;

    for d in ["erofs", "tmp"] {
        std::fs::create_dir_all(cache_dir.join(d))?;
    }

    let mut images = vec![ImageFile {
        name: NIX_CLOSURE_IMAGE.to_string(),
        path: PathBuf::from(NIX_CLOSURE_DIR).join(NIX_CLOSURE_IMAGE),
    }];
    let mut specs = Vec::new();
    for (name, locked) in &inputs {
        let store_path = nix_drv::source_store_path(&locked.nar_hash);
        let dest = image_path_for(&cache_dir, &store_path);
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

    let nix_root = std::fs::read_to_string(Path::new(NIX_CLOSURE_DIR).join("root"))?
        .trim()
        .to_string();
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
    spec.src_dir = Some(dir);
    let vm = Vm::boot(spec).context("booting eval vm")?;

    let rpc_cache_dir = cache_dir.clone();
    let eval_req = apis::FlakeEvalRequest {
        nix_image: NIX_CLOSURE_IMAGE.to_string(),
        nix_root,
        attr: flake_ref.attr.clone(),
        inputs: specs,
    };

    let outcome = vm
        .guest_rpc(move |c| async move {
            let text = c
                .flake_eval(crate::rpc::rpc_context(), eval_req)
                .await
                .map_err(|e| anyhow::anyhow!("flake_eval rpc: {e}"))?
                .map_err(anyhow::Error::msg)?;
            let drvs = {
                let _ = std::fs::write(rpc_cache_dir.join("last-eval.json"), &text);
                nix_drv::parse(&text).context("parsing derivation show output")?
            };
            let root = nix_drv::find_root(&drvs)
                .ok_or_else(|| anyhow!("no unique root derivation in eval output"))?;
            let closure = nix_drv::closure(&drvs, &root);
            let produced: std::collections::BTreeSet<&String> = closure
                .drvs
                .iter()
                .flat_map(|d| drvs[d].outputs.values().map(|o| &o.path))
                .collect();
            let mut missing = Vec::new();
            for p in &closure.store_paths {
                if produced.contains(p) {
                    continue;
                }
                if image_path_for(&rpc_cache_dir, p).exists() {
                    continue;
                }
                missing.push(p.clone());
            }
            let mut extra_images = BTreeMap::new();
            if missing.is_empty() {
                let _ = std::fs::remove_file(&scratch);
            } else {
                println!(
                    "flake: packing {} eval-created source paths from guest",
                    missing.len()
                );
                let size = c
                    .pack_store_paths(
                        crate::rpc::rpc_context(),
                        apis::PackPathsRequest {
                            paths: missing.clone(),
                            device: "/dev/vda".to_string(),
                        },
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("pack_store_paths rpc: {e}"))?
                    .map_err(anyhow::Error::msg)?;
                let f = std::fs::OpenOptions::new().write(true).open(&scratch)?;
                f.set_len(size)?;
                let dest = rpc_cache_dir
                    .join("erofs")
                    .join(format!("flake-extras-{}.erofs", apis::store_hash(&root)));
                std::fs::rename(&scratch, &dest)?;
                for p in &missing {
                    extra_images.insert(p.clone(), dest.clone());
                }
            }
            Ok((text, extra_images))
        })
        .await;
    vm.reap(std::time::Duration::from_secs(120)).await;
    let (drv_json, extra_images) = outcome?;

    crate::orchestrator::run(BuildOpts {
        drv_json,
        cache_url,
        cache_dir: cache_dir.clone(),
        extra_images,
    })
    .await
}

#[cfg(test)]
mod tests;
