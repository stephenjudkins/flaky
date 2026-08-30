//! Host-side build orchestration: resolve the closure, figure out which
//! store paths are available from a binary cache, and build whatever is
//! left (including the root) inside microVMs.
//!
//! Every store path gets its own EROFS image in `.cache/erofs/<hash>.erofs`
//! (volume name = first 16 chars of the hash). Build inputs are exposed as
//! regular files through one synthetic virtio-fs device and mounted by the
//! guest using file-backed EROFS.
//!
//! The set of inputs a build actually needs is the *reference closure* of
//! the drv's direct inputs: Nix's reference scanner guarantees that every
//! store path mentioned in a path's contents appears in its narinfo
//! `References:` line, so anything outside that closure is unreachable by
//! name and cannot be needed. For hello that's 65 paths instead of the
//! 531-path drv closure (the rest are bootstrap *build* inputs, already
//! compiled into the binaries we fetch).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use apis::{BuildRequest, BuildResult, InputSpec, OutputSpec};
use erofs_builder::{CreateOptions, DEFAULT_BLOCK_SIZE};
use futures::stream::{self, StreamExt};
use nix_cache::{CachedNar, Lookup, NixCache, StorePathHash};
use nix_drv::{Closure, Derivations};

use crate::fetchurl;
use crate::image_fs::ImageFile;
use crate::vm::{BlkDev, Vm, VmSpec};

const OUTPUT_DEVICE_SIZE: u64 = 4 << 30;
const OUTPUT_LABEL_OFFSET: u64 = 65536;
const MAX_CONCURRENT_FETCHES: usize = 8;
const MAX_OUTPUT_DEVICES: usize = 29;

pub struct BuildOpts {
    pub drv_json: PathBuf,
    pub cache_url: String,
    pub cache_dir: PathBuf,
}

struct Plan {
    /// Cache hits: store path -> narinfo summary (incl. references).
    hits: BTreeMap<String, CachedNar>,
    /// Paths not in the cache: output path -> producing drv path.
    miss_drv: BTreeMap<String, String>,
    /// Store paths to build (drv paths, dependencies first).
    to_build: Vec<String>,
}

struct Ctx<'a> {
    drvs: &'a Derivations,
    closure: &'a Closure,
    opts: &'a BuildOpts,
    cache: NixCache,
    plan: Plan,
    /// store path -> erofs image (pre-existing, fetched, or built here).
    images: BTreeMap<String, PathBuf>,
}

pub fn run(opts: BuildOpts) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(&opts.drv_json)
        .with_context(|| format!("reading {}", opts.drv_json.display()))?;
    let drvs = nix_drv::parse(&text).context("parsing derivation json")?;
    let root = nix_drv::find_root(&drvs).ok_or_else(|| anyhow!("no unique root derivation"))?;
    let closure = nix_drv::closure(&drvs, &root);
    let root_out: Option<String> = drvs[&root].outputs.values().next().map(|o| o.path.clone());
    println!(
        "build: root {root}, closure: {} drvs / {} store paths",
        closure.drvs.len(),
        closure.store_paths.len()
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut ctx = rt.block_on(async {
        let mut ctx = plan(&drvs, &closure, &root, &opts).await?;
        // prefetch images for the union of all builds' input sets
        let mut needed: BTreeSet<String> = BTreeSet::new();
        for dp in ctx.plan.to_build.clone() {
            needed.extend(ctx.input_set(&dp)?);
        }
        // ensure root outputs materialize even when the whole closure is cached
        needed.extend(drvs[&root].outputs.values().map(|o| o.path.clone()));
        let building: BTreeSet<String> = ctx.plan.to_build.clone().into_iter().collect();
        let fetchable: Vec<String> = needed
            .into_iter()
            .filter(|p| {
                if ctx.images.contains_key(&*p) {
                    return false;
                }
                match ctx.plan.miss_drv.get(&*p) {
                    Some(dp) => !building.contains(dp),
                    None => true,
                }
            })
            .collect();
        println!("build: fetching {} input images", fetchable.len());
        fetch_images(&mut ctx, fetchable).await?;
        anyhow::Ok(ctx)
    })?;
    // VM builds run outside the async runtime: Vm::run_build spins up its own
    for drv_path in ctx.plan.to_build.clone() {
        build_one(&mut ctx, &drv_path)?;
    }
    root_out.ok_or_else(|| anyhow!("root derivation has no outputs"))
}

async fn plan<'a>(
    drvs: &'a Derivations,
    closure: &'a Closure,
    root: &'a str,
    opts: &'a BuildOpts,
) -> anyhow::Result<Ctx<'a>> {
    let cache = NixCache::new(&opts.cache_url).context("creating cache client")?;

    for d in ["erofs", "build", "tmp"] {
        std::fs::create_dir_all(opts.cache_dir.join(d))?;
    }

    // images carried over from earlier sessions
    // look up narinfo for every fetchable path: the References lines drive
    // input-set computation even when the image is already cached
    let to_lookup: Vec<&String> = closure.store_paths.iter().collect();
    let mut images: BTreeMap<String, PathBuf> = BTreeMap::new();
    for p in &to_lookup {
        let img = image_path(opts, p);
        if img.exists() {
            images.insert((*p).clone(), img);
        }
    }
    println!(
        "build: {} images cached from earlier runs, looking up {} paths",
        images.len(),
        to_lookup.len()
    );

    let lookups = lookup_batched(&cache, &to_lookup).await?;
    let mut hits = BTreeMap::new();
    let mut miss_drv = BTreeMap::new();
    let mut to_build = BTreeSet::new();
    for (_i, p, lookup) in lookups {
        match lookup {
            Some(nar) => {
                hits.insert(p, nar);
            }
            None => {
                let dp = producing_drv(closure, drvs, &p)
                    .ok_or_else(|| anyhow!("no drv in closure produces {p}"))?;
                miss_drv.insert(p.clone(), dp.clone());
                if drvs[&dp].builder != "builtin:fetchurl" {
                    println!("[miss] {}: will build {}", nix_drv::basename(&p), dp);
                    to_build.insert(dp);
                } else {
                    println!(
                        "[miss] {}: builtin:fetchurl, will realize on demand",
                        nix_drv::basename(&p)
                    );
                }
            }
        }
    }

    Ok(Ctx {
        drvs,
        closure,
        opts,
        cache,
        plan: Plan {
            hits,
            miss_drv,
            to_build: topo_order(drvs, &to_build, root),
        },
        images,
    })
}

fn image_path(opts: &BuildOpts, store_path: &str) -> PathBuf {
    opts.cache_dir
        .join("erofs")
        .join(format!("{}.erofs", nix_drv::hash_part(store_path)))
}

fn producing_drv(closure: &Closure, drvs: &Derivations, path: &str) -> Option<String> {
    closure
        .drvs
        .iter()
        .find(|dp| drvs[dp.as_str()].outputs.values().any(|o| o.path == path))
        .cloned()
}

impl<'a> Ctx<'a> {
    /// The direct input paths of a drv: declared sources, the requested
    /// outputs of its input drvs, and anything named in builder/args/env
    /// (e.g. the `builder` path, which Nix does not require to be declared).
    fn drv_direct_inputs(&self, drv_path: &str) -> Vec<String> {
        let drv = &self.drvs[drv_path];
        let mut out: BTreeSet<String> = drv.inputSrcs.iter().cloned().collect();
        for (dep, spec) in &drv.inputDrvs {
            for o in &spec.outputs {
                if let Some(outp) = self.drvs[dep].outputs.get(o) {
                    out.insert(outp.path.clone());
                }
            }
        }
        let mut text = String::new();
        text.push_str(&drv.builder);
        for a in &drv.args {
            text.push(' ');
            text.push_str(a);
        }
        for v in drv.env.values() {
            text.push(' ');
            text.push_str(v);
        }
        out.extend(scan_store_paths(&text));
        out.into_iter().collect()
    }

    /// Paths referenced *by the contents* of a store path. For cache hits
    /// the narinfo `References:` list is exact; otherwise (built here, or
    /// carried over from an earlier session) fall back to the producing
    /// drv's inputs, which Nix guarantees to be a superset of the output's
    /// references.
    fn refs_of(&self, path: &str) -> Vec<String> {
        if let Some(nar) = self.plan.hits.get(path) {
            return nar
                .references
                .iter()
                .map(|name| format!("/nix/store/{name}"))
                .collect();
        }
        let dp = self
            .plan
            .miss_drv
            .get(path)
            .cloned()
            .or_else(|| producing_drv(self.closure, self.drvs, path));
        let Some(dp) = dp else {
            return Vec::new();
        };
        if self.drvs[&dp].builder == "builtin:fetchurl" {
            return Vec::new();
        }
        self.drv_direct_inputs(&dp)
    }

    /// The transitive input set for building `drv_path`: its direct inputs
    /// plus everything those reference by content. The drv's own outputs
    /// are excluded (the guest mounts those as tmpfs build targets).
    fn input_set(&self, drv_path: &str) -> anyhow::Result<Vec<String>> {
        let own: BTreeSet<&String> = self.drvs[drv_path]
            .outputs
            .values()
            .map(|o| &o.path)
            .collect();
        let mut seen: BTreeSet<String> = self
            .drv_direct_inputs(drv_path)
            .into_iter()
            .filter(|p| !own.contains(p))
            .collect();
        let mut queue: VecDeque<String> = seen.iter().cloned().collect();
        while let Some(p) = queue.pop_front() {
            for r in self.refs_of(&p) {
                if !own.contains(&r) && seen.insert(r.clone()) {
                    queue.push_back(r);
                }
            }
        }
        Ok(seen.into_iter().collect())
    }
}

fn topo_order(drvs: &Derivations, to_build: &BTreeSet<String>, root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    fn visit(
        drvs: &Derivations,
        to_build: &BTreeSet<String>,
        p: &str,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(p.to_string()) {
            return;
        }
        if let Some(d) = drvs.get(p) {
            for dep in d.inputDrvs.keys() {
                visit(drvs, to_build, dep, seen, out);
            }
        }
        if to_build.contains(p) && !out.iter().any(|e| e == p) {
            out.push(p.to_string());
        }
    }
    visit(drvs, to_build, root, &mut seen, &mut out);
    out
}

/// Store paths mentioned in a string (builder, args, env values). Store
/// path names may only contain [A-Za-z0-9+._?=-], so anything else ends
/// the match. Subpaths are truncated to the store path root: we match
/// `/nix/store/<name>/bin/bash` but return `/nix/store/<name>`.
fn scan_store_paths(s: &str) -> Vec<String> {
    fn name_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'_' | b'.' | b'?' | b'=')
    }
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = s[i..].find("/nix/store/") {
        let start = i + rel;
        let mut end = start + "/nix/store/".len();
        while end < b.len() && name_char(b[end]) {
            end += 1;
        }
        if end > start + "/nix/store/".len() {
            out.push(s[start..end].to_string());
        }
        i = end.max(i + 1);
    }
    out
}

async fn lookup_batched(
    cache: &NixCache,
    queue: &[&String],
) -> anyhow::Result<Vec<(usize, String, Option<CachedNar>)>> {
    let results = stream::iter(queue.iter().enumerate())
        .map(|(i, p)| {
            let cache = cache.clone();
            async move {
                let hash = StorePathHash::from_store_path(p)?;
                let r = match cache.lookup(&hash).await {
                    Ok(Lookup::Hit(nar)) => Some(nar),
                    Ok(Lookup::Miss) => None,
                    Err(e) => return Err(anyhow::Error::from(e)),
                };
                Ok((i, p.as_str(), r))
            }
        })
        .buffer_unordered(MAX_CONCURRENT_FETCHES)
        .collect::<Vec<anyhow::Result<(usize, &str, Option<CachedNar>)>>>()
        .await;
    let mut out = Vec::new();
    for r in results {
        let (i, p, l) = r?;
        out.push((i, p.to_string(), l));
    }
    out.sort_by_key(|(i, _, _)| *i);
    Ok(out)
}

/// One EROFS image per store path, volume name = hash[..16].
async fn fetch_one_image(
    cache: &NixCache,
    store_path: &str,
    nar: &CachedNar,
    dest: &Path,
) -> anyhow::Result<()> {
    let base = nix_drv::basename(store_path);
    let file = tokio::fs::File::create(dest).await?;
    let sink = nar_to_erofs::CountingSink::new(file);
    let mut writer = erofs_builder::Writer::new(
        sink,
        CreateOptions {
            block_size: DEFAULT_BLOCK_SIZE,
            build_time: 0,
            build_time_nsec: 0,
            volume_name: nix_drv::hash_part(store_path)[..16].to_string(),
            ..Default::default()
        },
    )
    .await
    .with_context(|| format!("creating image writer for {base}"))?;
    nix_cache::fetch_nar_into(cache, nar, &mut writer, base)
        .await
        .with_context(|| format!("fetching {store_path}"))?;
    let sink = writer.finish().await?;
    let size = sink.count();
    let file = sink.into_inner();
    file.set_len(size).await?;
    file.sync_all().await?;
    println!(
        "fetched {} (nar {} bytes -> image {} bytes)",
        base, nar.nar_size, size
    );
    Ok(())
}

async fn fetch_images(ctx: &mut Ctx<'_>, paths: Vec<String>) -> anyhow::Result<()> {
    let total = paths.len();
    let results = stream::iter(paths.into_iter().enumerate())
        .map(|(i, p)| {
            let ctx_drvs = ctx.drvs;
            let cache = ctx.cache.clone();
            let hits = &ctx.plan.hits;
            let miss_drv = &ctx.plan.miss_drv;
            let opts = ctx.opts;
            async move {
                let dest = image_path(opts, &p);
                let img = if let Some(nar) = hits.get(&p) {
                    fetch_one_image(&cache, &p, nar, &dest).await?;
                    dest
                } else if let Some(dp) = miss_drv.get(&p) {
                    if ctx_drvs[dp].builder != "builtin:fetchurl" {
                        anyhow::bail!("{p} is an output of {dp} which has not been built");
                    }
                    let n = fetchurl::realize_to_image(
                        &ctx_drvs[dp],
                        &dest,
                        &opts.cache_dir.join("tmp"),
                    )
                    .await
                    .with_context(|| format!("realizing fetchurl for {p}"))?;
                    println!(
                        "[{}/{}] realized fetchurl {} ({} bytes)",
                        i + 1,
                        total,
                        nix_drv::basename(&p),
                        n
                    );
                    dest
                } else {
                    anyhow::bail!("no source for input {p}");
                };
                Ok((p, img))
            }
        })
        .buffer_unordered(MAX_CONCURRENT_FETCHES)
        .collect::<Vec<anyhow::Result<(String, PathBuf)>>>()
        .await;
    let mut done = 0;
    for r in results {
        let (p, img) = r?;
        done += 1;
        ctx.images.insert(p, img);
    }
    println!("build: fetched {done} images");
    Ok(())
}

fn prep_output_device(path: &Path, output_name: &str) -> anyhow::Result<()> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    f.set_len(0)?;
    f.set_len(OUTPUT_DEVICE_SIZE)?;
    let mut label = vec![0u8; 64];
    let l = format!("flaky-out:{output_name}");
    label[..l.len()].copy_from_slice(l.as_bytes());
    use std::io::{Seek, SeekFrom, Write};
    let mut f = f;
    f.seek(SeekFrom::Start(OUTPUT_LABEL_OFFSET))?;
    f.write_all(&label)?;
    f.sync_all()?;
    Ok(())
}

fn build_one(ctx: &mut Ctx<'_>, drv_path: &str) -> anyhow::Result<()> {
    let drv = ctx.drvs[drv_path].clone();
    let outputs: Vec<_> = drv.outputs.iter().collect();
    println!(
        "build: {} ({} outputs, system {})",
        drv_path,
        outputs.len(),
        drv.system
    );

    let inputs = ctx.input_set(drv_path)?;
    println!("build: {} inputs", inputs.len());
    if outputs.len() > MAX_OUTPUT_DEVICES {
        bail!(
            "{drv_path} needs {} output devices, PCI topology supports at most {MAX_OUTPUT_DEVICES}",
            outputs.len()
        );
    }

    let mut images = Vec::with_capacity(inputs.len());
    let mut blk = Vec::with_capacity(outputs.len());
    let mut input_specs = Vec::with_capacity(inputs.len());
    for p in &inputs {
        let path = ctx
            .images
            .get(p)
            .cloned()
            .ok_or_else(|| anyhow!("missing image for input {p}"))?;
        images.push(ImageFile {
            name: format!("{}.erofs", nix_drv::hash_part(p)),
            path,
        });
        input_specs.push(InputSpec {
            store_path: p.clone(),
            volume_id: nix_drv::hash_part(p)[..16].to_string(),
        });
    }

    let mut out_devices: Vec<(String, String, PathBuf)> = Vec::new();
    for (name, out) in &outputs {
        let dev_path = ctx.opts.cache_dir.join("build").join(format!(
            "{}-{name}.img",
            &nix_drv::hash_part(&out.path)[..16]
        ));
        prep_output_device(&dev_path, name)
            .with_context(|| format!("preparing output device for {name}"))?;
        blk.push(BlkDev {
            path: dev_path.clone(),
            readonly: false,
        });
        out_devices.push((name.to_string(), out.path.clone(), dev_path));
    }

    let request = BuildRequest {
        drv_path: drv_path.to_string(),
        builder: drv.builder.clone(),
        args: drv.args.clone(),
        env: drv
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        outputs: outputs
            .iter()
            .map(|(name, out)| OutputSpec {
                name: name.to_string(),
                store_path: out.path.clone(),
            })
            .collect(),
        inputs: input_specs,
    };

    let vm = Vm::boot(VmSpec {
        mem_mib: 1024,
        cpus: 1,
        images,
        blk,
    })
    .context("booting build vm")?;
    let result: BuildResult = vm.run_build(request).context("running build in vm")?;
    reap_vm(vm, Duration::from_secs(60));

    if !result.success {
        bail!(
            "build of {drv_path} failed: exit {:?} error {:?}",
            result.exit_code,
            result.error
        );
    }

    for img in &result.outputs {
        let (_, out_path, dev_path) = out_devices
            .iter()
            .find(|(n, _, _)| n == &img.name)
            .ok_or_else(|| anyhow!("guest reported unknown output {}", img.name))?;
        let f = std::fs::OpenOptions::new().write(true).open(dev_path)?;
        f.set_len(img.image_size)
            .context("truncating output image")?;
        let dest = image_path(ctx.opts, out_path);
        if dest.exists() {
            std::fs::remove_file(&dest)?;
        }
        std::fs::rename(dev_path, &dest)
            .with_context(|| format!("saving output image to {}", dest.display()))?;
        println!(
            "build: output {} -> {} ({} bytes)",
            out_path,
            dest.display(),
            img.image_size
        );
        ctx.images.insert(out_path.clone(), dest);
    }
    Ok(())
}

fn reap_vm(vm: Vm, timeout: Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(vm.wait());
    });
    match rx.recv_timeout(timeout) {
        Ok(r) => {
            if let Err(e) = r {
                eprintln!("warning: vm exited with error: {e:#}");
            }
        }
        Err(_) => eprintln!(
            "warning: vm did not power off within {timeout:?}; continuing with vm still running"
        ),
    }
}

mod tests {
    use super::*;

    #[test]
    fn scans_store_paths_from_env_text() {
        let text = "builder=/nix/store/ki4g5pbmqcs07mwrxmvik6dpnzyx3z4c-bootstrap-tools/bin/bash \
                    PATH=/nix/store/aaa1111aaa1111aaa1111aaa1111aa11-x/bin:/nix/store/bbb2222bbb2222bbb2222bbb2222bb22-y/bin \
                    flags='--with-pic' json=\"{\\\"stdenv\\\": \\\"/nix/store/ccc3333ccc3333ccc3333ccc3333cc33-z\\\"}\"";
        let paths = scan_store_paths(text);
        assert_eq!(
            paths,
            vec![
                "/nix/store/ki4g5pbmqcs07mwrxmvik6dpnzyx3z4c-bootstrap-tools",
                "/nix/store/aaa1111aaa1111aaa1111aaa1111aa11-x",
                "/nix/store/bbb2222bbb2222bbb2222bbb2222bb22-y",
                "/nix/store/ccc3333ccc3333ccc3333ccc3333cc33-z",
            ]
        );
    }

    #[test]
    fn scan_rejects_short_and_terminated() {
        assert!(scan_store_paths("no paths here /nix/store").is_empty());
        // name chars only after the prefix; punctuation terminates
        let v = scan_store_paths("x/nix/store/abc123/bin/sh foo");
        assert_eq!(v, vec!["/nix/store/abc123"]);
    }
}
