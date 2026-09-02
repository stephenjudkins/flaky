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

fn image_path(opts: &BuildOpts, store_path: &str) -> PathBuf {
    opts.cache_dir
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

    let mut ctx = plan::plan(&drvs, &closure, &root, &opts).await?;
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
    fetch::fetch_images(&mut ctx, fetchable).await?;
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
mod tests {
    use super::scan_store_paths;

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
