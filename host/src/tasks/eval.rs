//! Guest nix evaluation: flake evaluation in a microVM, and fetching the
//! flake's locked source inputs as hash-addressed EROFS images.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use futures::TryStreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::flake::{Locked, NIX_CLOSURE_DIR, NIX_CLOSURE_IMAGE, expected_nar_hash};
use crate::image_fs::ImageFile;
use crate::nar::{DirNar, HashReader};
use crate::orchestrator::image_path_for;
use crate::task::{Context, Task, TaskFuture};
use crate::tasks::VerifyNarHash;
use crate::vm::{BlkDev, VmSpec};

const EXTRAS_DEVICE_SIZE: u64 = 8 << 30;

async fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
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

async fn stream_fetch(
    url: &str,
    top: &str,
    base: &str,
    volume: &str,
    tmp_img: &Path,
) -> Result<crate::tarball::ImageWriter, String> {
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

/// Fetches the github tarball for a locked flake input, streams it into a
/// hash-addressed EROFS image, and verifies its NAR hash against the lock.
pub struct FetchInputImage {
    pub input_name: String,
    pub locked: Locked,
    pub store_path: String,
}

impl Task for FetchInputImage {
    type Output = PathBuf;

    fn run(self: Box<Self>, ctx: Context) -> TaskFuture<PathBuf> {
        Box::pin(async move {
            let _permit = ctx.fetch_permit().await?;
            let locked = self.locked;
            let url = format!(
                "https://github.com/{}/{}/archive/{}.tar.gz",
                locked.owner, locked.repo, locked.rev
            );
            eprintln!("flake: fetching input {} ({url})", self.input_name);
            let dest = ctx.image_path(&self.store_path);
            let tmp_dir = ctx
                .tmp_dir()
                .join(format!("input-{}", apis::store_hash(&self.store_path)));
            let base = nix_drv::basename(&self.store_path).to_string();
            let volume = apis::volume_id(&self.store_path);
            let mut tmp_img = dest.as_os_str().to_owned();
            tmp_img.push(".download");
            let tmp_img = PathBuf::from(tmp_img);
            let top = format!("{}-{}", locked.repo, locked.rev);
            let result = async {
                let writer = match stream_fetch(&url, &top, &base, &volume, &tmp_img).await {
                    Ok(w) => w,
                    Err(reason) => {
                        eprintln!("flake: streaming fetch fell back to disk: {reason}");
                        fetch_via_disk(&url, &top, &tmp_dir, &base, &volume, &tmp_img).await?
                    }
                };
                let (file, size) = nar_to_erofs::finish_image(writer).await?;
                file.set_len(size).await?;
                file.sync_all().await?;
                drop(file);
                ctx.spawn(VerifyNarHash {
                    image: tmp_img.clone(),
                    expected: expected_nar_hash(&locked.nar_hash)?,
                    label: format!("{}/{}", locked.owner, locked.repo),
                })
                .await?;
                std::fs::rename(&tmp_img, &dest)?;
                println!(
                    "flake: fetched {}/{} (image {} bytes)",
                    locked.owner, locked.repo, size
                );
                anyhow::Ok(())
            }
            .await;
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp_img);
            }
            result.map(|()| dest)
        })
    }
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

fn prep_scratch(path: &Path, size: u64) -> anyhow::Result<()> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.set_len(size)?;
    Ok(())
}

/// Evaluates `dir#attrs` with nix in a microVM. Locked inputs are fetched
/// first (as dependent tasks); source paths nix creates during eval are
/// packed by the guest into one shared extras image.
pub struct EvalFlake {
    pub dir: PathBuf,
    pub inputs: Vec<(String, Locked)>,
    pub attrs: Vec<String>,
    pub nix_root: String,
}

pub struct EvalOutcome {
    pub drv_jsons: BTreeMap<String, String>,
    /// attr -> eval-created source paths covered by the extras image.
    pub missing: BTreeMap<String, Vec<String>>,
    pub extra_images: BTreeMap<String, PathBuf>,
    pub extras_image: Option<String>,
}

impl Task for EvalFlake {
    type Output = EvalOutcome;

    fn run(self: Box<Self>, ctx: Context) -> TaskFuture<EvalOutcome> {
        Box::pin(async move {
            let mut fetches = Vec::new();
            let mut resolved = Vec::new();
            for (name, locked) in &self.inputs {
                let store_path = nix_drv::source_store_path(&locked.nar_hash);
                let dest = ctx.image_path(&store_path);
                if !dest.exists() {
                    fetches.push(ctx.spawn(FetchInputImage {
                        input_name: name.clone(),
                        locked: locked.clone(),
                        store_path: store_path.clone(),
                    }));
                } else {
                    eprintln!("flake: input {name} already cached");
                }
                resolved.push((name.clone(), store_path, dest));
            }
            for fut in fetches {
                fut.await?;
            }

            let mut images = vec![ImageFile {
                name: NIX_CLOSURE_IMAGE.to_string(),
                path: PathBuf::from(NIX_CLOSURE_DIR).join(NIX_CLOSURE_IMAGE),
            }];
            let mut specs = Vec::new();
            for (name, store_path, dest) in resolved {
                images.push(ImageFile {
                    name: apis::image_name(&store_path),
                    path: dest,
                });
                specs.push(apis::FlakeInputSpec {
                    name,
                    image: apis::image_name(&store_path),
                    store_path,
                });
            }

            let scratch = ctx.tmp_dir().join("flake-extras.img");
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
            spec.src_dir = Some(self.dir.clone());
            let vm = ctx.boot_vm(spec).context("booting eval vm")?;

            let req = apis::FlakeEvalRequest {
                nix_image: NIX_CLOSURE_IMAGE.to_string(),
                nix_root: self.nix_root.clone(),
                attrs: self.attrs.clone(),
                inputs: specs,
            };

            let rpc_cache_dir = ctx.cache_dir().to_path_buf();
            let rpc_attrs = self.attrs.clone();
            let outcome = vm
                .guest_rpc(move |c| async move {
                    let text = c
                        .flake_eval(crate::rpc::rpc_context(), req)
                        .await
                        .map_err(|e| anyhow!("flake_eval rpc: {e}"))?
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
                    let drvs =
                        nix_drv::parse(&show_text).context("parsing derivation show output")?;
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
                            None => nix_drv::find_root(&drvs).ok_or_else(|| {
                                anyhow!("no unique root derivation in eval output")
                            })?,
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
                            .map_err(|e| anyhow!("pack_store_paths rpc: {e}"))?
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
            let (per_attr, extra_images, extras_image) = outcome?;

            let mut drv_jsons = BTreeMap::new();
            let mut missing = BTreeMap::new();
            for (attr, (text, m)) in per_attr {
                drv_jsons.insert(attr.clone(), text);
                missing.insert(attr, m);
            }
            Ok(EvalOutcome {
                drv_jsons,
                missing,
                extra_images,
                extras_image,
            })
        })
    }
}

fn hex(d: [u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}
