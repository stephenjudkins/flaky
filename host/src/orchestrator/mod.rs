//! Host-side build orchestration: figure out which missing store paths
//! can be fetched from a binary cache and which derivations must be built
//! in microVMs. Narinfo lookups are lazy: a path is only looked up at the
//! moment we must materialize it and its EROFS image is absent, so a warm
//! cache does zero network requests.
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

mod exec;
mod fetch;
mod plan;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Context as _, anyhow};
use nix_cache::{CachedNar, NixCache};
use nix_drv::{Closure, Derivations};

const MAX_CONCURRENT_FETCHES: usize = 8;

pub struct BuildOpts {
    /// `nix derivation show -r` JSON text.
    pub drv_json: String,
    pub cache_url: String,
    pub cache_dir: PathBuf,
    /// Store paths realized outside the normal fetch/build flow (e.g.
    /// sources created by nix during flake eval) -> the erofs image
    /// containing them as top-level entries.
    pub extra_images: BTreeMap<String, PathBuf>,
}

struct Plan {
    /// Narinfo summaries (incl. references): preloaded from the local
    /// narinfo cache for image-present paths, plus everything fetched or
    /// looked up this run. Also drives `refs_of`.
    hits: BTreeMap<String, CachedNar>,
    /// Missing paths available upstream: store path -> narinfo.
    fetches: BTreeMap<String, CachedNar>,
    /// Missing paths realized from builtin:fetchurl drvs: path -> drv.
    fetchurl: BTreeMap<String, String>,
    /// Drvs to build (dependencies first).
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

fn image_path(opts: &BuildOpts, store_path: &str) -> PathBuf {
    opts.cache_dir
        .join("erofs")
        .join(format!("{}.erofs", apis::store_hash(store_path)))
}

pub(crate) fn image_path_for(cache_dir: &std::path::Path, store_path: &str) -> PathBuf {
    cache_dir
        .join("erofs")
        .join(format!("{}.erofs", apis::store_hash(store_path)))
}

fn producing_drv(closure: &Closure, drvs: &Derivations, path: &str) -> Option<String> {
    closure
        .drvs
        .iter()
        .find(|dp| drvs[dp.as_str()].outputs.values().any(|o| o.path == path))
        .cloned()
}

pub async fn run(opts: BuildOpts) -> anyhow::Result<String> {
    let drvs = nix_drv::parse(&opts.drv_json).context("parsing derivation json")?;
    let root = nix_drv::find_root(&drvs).ok_or_else(|| anyhow!("no unique root derivation"))?;
    let closure = nix_drv::closure(&drvs, &root);
    let root_out: Option<String> = drvs[&root].outputs.values().next().map(|o| o.path.clone());
    println!(
        "build: root {root}, closure: {} drvs / {} store paths",
        closure.drvs.len(),
        closure.store_paths.len()
    );

    let mut ctx = plan::plan(&drvs, &closure, &root, &opts).await?;
    fetch::fetch_images(&mut ctx).await?;
    for drv_path in ctx.plan.to_build.clone() {
        exec::build_one(&mut ctx, &drv_path).await?;
    }
    root_out.ok_or_else(|| anyhow!("root derivation has no outputs"))
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
        let dp = producing_drv(self.closure, self.drvs, path);
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

#[cfg(test)]
mod tests;
