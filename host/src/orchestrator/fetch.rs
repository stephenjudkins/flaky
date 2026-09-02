//! Fetching input images: cache-hit NARs become EROFS images, and
//! `builtin:fetchurl` derivations are realized on demand.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use futures::stream::{self, StreamExt};
use nix_cache::NixCache;

use super::{Ctx, image_path};
use crate::fetchurl;

/// One EROFS image per store path, volume name = hash[..16].
async fn fetch_one_image(
    cache: &NixCache,
    store_path: &str,
    nar: &nix_cache::CachedNar,
    dest: &Path,
) -> anyhow::Result<()> {
    let base = nix_drv::basename(store_path);
    let file = tokio::fs::File::create(dest).await?;
    let mut writer = nar_to_erofs::image_writer(file, &apis::volume_id(store_path))
        .await
        .with_context(|| format!("creating image writer for {base}"))?;
    nix_cache::fetch_nar_into(cache, nar, &mut writer, base)
        .await
        .with_context(|| format!("fetching {store_path}"))?;
    let (file, size) = nar_to_erofs::finish_image(writer).await?;
    file.set_len(size).await?;
    file.sync_all().await?;
    println!(
        "fetched {} (nar {} bytes -> image {} bytes)",
        base, nar.nar_size, size
    );
    Ok(())
}

pub(super) async fn fetch_images(ctx: &mut Ctx<'_>) -> anyhow::Result<()> {
    let Ctx {
        drvs,
        cache,
        plan,
        opts,
        ..
    } = &*ctx;
    let total = plan.fetches.len() + plan.fetchurl.len();
    println!("build: fetching {total} input images");

    let downloads = stream::iter(plan.fetches.iter().map(|(p, nar)| {
        let cache = cache.clone();
        let p = p.clone();
        let nar = nar.clone();
        async move {
            let dest = image_path(opts, &p);
            fetch_one_image(&cache, &p, &nar, &dest).await?;
            Ok((p, dest))
        }
    }))
    .buffer_unordered(super::MAX_CONCURRENT_FETCHES)
    .collect::<Vec<anyhow::Result<(String, PathBuf)>>>()
    .await;

    let realizes = stream::iter(plan.fetchurl.iter().map(|(p, dp)| {
        let dp = dp.clone();
        let p = p.clone();
        async move {
            let dest = image_path(opts, &p);
            let n = fetchurl::realize_to_image(&drvs[&dp], &dest, &opts.cache_dir.join("tmp"))
                .await
                .with_context(|| format!("realizing fetchurl for {p}"))?;
            println!("realized fetchurl {} ({} bytes)", nix_drv::basename(&p), n);
            Ok((p, dest))
        }
    }))
    .buffer_unordered(super::MAX_CONCURRENT_FETCHES)
    .collect::<Vec<anyhow::Result<(String, PathBuf)>>>()
    .await;

    let mut done = 0;
    for r in downloads.into_iter().chain(realizes) {
        let (p, img) = r?;
        done += 1;
        ctx.images.insert(p, img);
    }
    println!("build: fetched {done} images");
    Ok(())
}
