//! Closure planning, lazily: only paths that actually need materializing
//! (their EROFS image is absent) are looked up, fetched, or built.
//!
//! The walk starts at the root drv's outputs. A missing path is looked up
//! once: an upstream hit is queued for download, a miss marks its producing
//! drv for building (which enqueues that drv's direct inputs), and a
//! `builtin:fetchurl` drv is queued for on-demand realization. Paths that
//! are inputs of a build also expand their references (from narinfo when
//! available, otherwise the producing drv's inputs as a superset), since
//! the guest must mount the full reference closure of every direct input.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use anyhow::anyhow;
use nix_cache::{Lookup, StorePathHash};
use nix_drv::{Closure, Derivations};

use super::{Plan, Workspace, producing_drv};
use crate::task::Context;

pub(super) async fn plan<'a>(
    ctx: &Context,
    drvs: &'a Derivations,
    closure: &'a Closure,
    root: &'a str,
    extra_images: &BTreeMap<String, PathBuf>,
    expand_roots: bool,
) -> anyhow::Result<Workspace<'a>> {
    // fast path: when every output of the root drv already has a local
    // image, the build goal is already realized — no lookups or VM builds
    // can be needed (delete an output image in .cache/erofs to force a
    // rebuild). Closure goals always plan fully: the root's runtime
    // references may still be missing images.
    if !expand_roots
        && drvs[root]
            .outputs
            .values()
            .all(|o| ctx.image_path(&o.path).exists())
    {
        let images = drvs[root]
            .outputs
            .values()
            .map(|o| (o.path.clone(), ctx.image_path(&o.path)))
            .collect();
        println!("build: root outputs already cached locally");
        return Ok(Workspace {
            drvs,
            closure,
            plan: Plan {
                hits: BTreeMap::new(),
                fetches: BTreeMap::new(),
                fetchurl: BTreeMap::new(),
                to_build: Vec::new(),
            },
            images,
        });
    }

    // preload existing images; disk-cached narinfo gives exact references
    // for them (narinfo is never needed on the network for these — a
    // locally built path has no upstream narinfo anyway)
    let mut images: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut hits = BTreeMap::new();
    for p in &closure.store_paths {
        let img = ctx.image_path(p);
        if !img.exists() {
            continue;
        }
        images.insert(p.clone(), img);
        if let Ok(hash) = StorePathHash::from_store_path(p) {
            if let Ok(Some(Lookup::Hit(nar))) = ctx.nix_cache().lookup_local(&hash) {
                hits.insert(p.clone(), nar);
            }
        }
    }
    println!("build: {} images cached from earlier runs", images.len());
    for (p, img) in extra_images {
        images.entry(p.clone()).or_insert_with(|| img.clone());
    }

    let mut ws = Workspace {
        drvs,
        closure,
        plan: Plan {
            hits,
            fetches: BTreeMap::new(),
            fetchurl: BTreeMap::new(),
            to_build: Vec::new(),
        },
        images,
    };
    resolve(&mut ws, ctx, root, expand_roots).await?;
    Ok(ws)
}

async fn resolve(
    ws: &mut Workspace<'_>,
    ctx: &Context,
    root: &str,
    expand_roots: bool,
) -> anyhow::Result<()> {
    let mut to_build: BTreeSet<String> = BTreeSet::new();
    // (store path, expand references?) — expansion applies to inputs of
    // builds and, for closure goals, to the root output itself; otherwise
    // the root outputs are the goal, not build inputs
    let mut frontier: VecDeque<(String, bool)> = match expand_roots {
        true => super::primary_output(ws.drvs, root)
            .map(|p| VecDeque::from([(p, true)]))
            .unwrap_or_default(),
        false => ws.drvs[root]
            .outputs
            .values()
            .map(|o| (o.path.clone(), false))
            .collect(),
    };
    let mut done: BTreeSet<String> = BTreeSet::new();

    while let Some((p, expand)) = frontier.pop_front() {
        if !done.insert(p.clone()) {
            continue;
        }
        if ws.images.contains_key(&p)
            || ws.plan.fetches.contains_key(&p)
            || ws.plan.fetchurl.contains_key(&p)
        {
            // already materialized (or queued); fall through to expansion
        } else {
            let hash = StorePathHash::from_store_path(&p)?;
            match ctx.nix_cache().lookup(&hash).await {
                Ok(Lookup::Hit(nar)) => {
                    println!("[hit] {}", nix_drv::basename(&p));
                    ws.plan.hits.insert(p.clone(), nar.clone());
                    ws.plan.fetches.insert(p.clone(), nar);
                }
                Ok(Lookup::Miss) => {
                    let dp = producing_drv(ws.closure, ws.drvs, &p)
                        .ok_or_else(|| anyhow!("no drv in closure produces {p}"))?;
                    if ws.drvs[&dp].builder == "builtin:fetchurl" {
                        println!(
                            "[miss] {}: builtin:fetchurl, will realize on demand",
                            nix_drv::basename(&p)
                        );
                        ws.plan.fetchurl.insert(p.clone(), dp.clone());
                    } else if to_build.insert(dp.clone()) {
                        println!("[miss] {}: will build {}", nix_drv::basename(&p), dp);
                        for inp in ws.drv_direct_inputs(&dp) {
                            frontier.push_back((inp, true));
                        }
                    }
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("looking up {}", nix_drv::basename(&p))));
                }
            }
        }
        if expand {
            for r in ws.refs_of(&p) {
                frontier.push_back((r, true));
            }
        }
    }

    println!(
        "build: {} to fetch, {} fetchurl, {} to build",
        ws.plan.fetches.len(),
        ws.plan.fetchurl.len(),
        to_build.len()
    );
    ws.plan.to_build = topo_order(ws.drvs, &to_build, root);
    Ok(())
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
