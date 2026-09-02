//! Closure planning: which store paths come from the binary cache, and
//! which derivations must be built.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, anyhow};
use futures::stream::{self, StreamExt};
use nix_cache::{Lookup, StorePathHash};
use nix_drv::{Closure, Derivations};

use super::{Ctx, Plan, image_path, producing_drv};
use crate::orchestrator::BuildOpts;

pub(super) async fn plan<'a>(
    drvs: &'a Derivations,
    closure: &'a Closure,
    root: &'a str,
    opts: &'a BuildOpts,
) -> anyhow::Result<Ctx<'a>> {
    let cache = nix_cache::NixCache::new(&opts.cache_url).context("creating cache client")?;

    for d in ["erofs", "build", "tmp"] {
        std::fs::create_dir_all(opts.cache_dir.join(d))?;
    }

    // fast path: when every output of the root drv already has a local
    // image, the build goal is already realized — no lookups or VM builds
    // can be needed (delete an output image in .cache/erofs to force a
    // rebuild).
    let mut images: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
    if drvs[root]
        .outputs
        .values()
        .all(|o| image_path(opts, &o.path).exists())
    {
        for o in drvs[root].outputs.values() {
            images.insert(o.path.clone(), image_path(opts, &o.path));
        }
        println!("build: root outputs already cached locally");
        return Ok(Ctx {
            drvs,
            closure,
            opts,
            cache,
            plan: Plan {
                hits: BTreeMap::new(),
                miss_drv: BTreeMap::new(),
                to_build: Vec::new(),
            },
            images,
        });
    }

    // local images are preloaded before lookup so builds can reuse them;
    // narinfo is still fetched for every path (including cached ones) because
    // the References lines keep input sets exact — the superset fallback
    // via drv inputs would pull in unneeded build-time deps
    let to_lookup: Vec<&String> = closure.store_paths.iter().collect();
    let mut images: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
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
                // outputs realized by an earlier run: keep the image, skip the build
                if drvs[&dp]
                    .outputs
                    .values()
                    .all(|o| images.contains_key(&o.path))
                {
                    println!(
                        "[cache] {}: output image already present",
                        nix_drv::basename(&p)
                    );
                    continue;
                }
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

async fn lookup_batched(
    cache: &nix_cache::NixCache,
    queue: &[&String],
) -> anyhow::Result<Vec<(usize, String, Option<nix_cache::CachedNar>)>> {
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
        .buffer_unordered(super::MAX_CONCURRENT_FETCHES)
        .collect::<Vec<anyhow::Result<(usize, &str, Option<nix_cache::CachedNar>)>>>()
        .await;
    let mut out = Vec::new();
    for r in results {
        let (i, p, l) = r?;
        out.push((i, p.to_string(), l));
    }
    out.sort_by_key(|(i, _, _)| *i);
    Ok(out)
}
