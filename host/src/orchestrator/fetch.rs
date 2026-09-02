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

pub(super) async fn fetch_images(ctx: &mut Ctx<'_>, paths: Vec<String>) -> anyhow::Result<()> {
    let total = paths.len();
    let Ctx {
        drvs,
        cache,
        plan,
        opts,
        ..
    } = &*ctx;
    let results = stream::iter(paths.into_iter().enumerate())
        .map(|(i, p)| {
            let cache = cache.clone();
            async move {
                let dest = image_path(opts, &p);
                let img = if let Some(nar) = plan.hits.get(&p) {
                    fetch_one_image(&cache, &p, nar, &dest).await?;
                    dest
                } else if let Some(dp) = plan.miss_drv.get(&p) {
                    if drvs[dp].builder != "builtin:fetchurl" {
                        anyhow::bail!("{p} is an output of {dp} which has not been built");
                    }
                    let n =
                        fetchurl::realize_to_image(&drvs[dp], &dest, &opts.cache_dir.join("tmp"))
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
        .buffer_unordered(super::MAX_CONCURRENT_FETCHES)
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
