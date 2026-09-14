//! Fetch tasks: binary-cache NARs streamed into EROFS images, and
//! host-side realization of `builtin:fetchurl` derivations.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use nix_cache::{CachedNar, NixCache};
use nix_drv::Derivation;

use crate::task::{Context, Task, TaskFuture};
use crate::tasks::VerifyNarHash;

/// One EROFS image per store path, volume name = hash[..16].
async fn fetch_one_image(
    cache: &NixCache,
    store_path: &str,
    nar: &CachedNar,
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

/// Fetches a cache-hit substitution and streams it into its EROFS image,
/// verifying the image's NAR hash against the narinfo before use.
pub struct FetchNar {
    pub store_path: String,
    pub nar: CachedNar,
}

impl Task for FetchNar {
    type Output = PathBuf;

    fn run(self: Box<Self>, ctx: Context) -> TaskFuture<PathBuf> {
        Box::pin(async move {
            let _permit = ctx.fetch_permit().await?;
            let dest = ctx.image_path(&self.store_path);
            fetch_one_image(ctx.nix_cache(), &self.store_path, &self.nar, &dest).await?;
            ctx.spawn(VerifyNarHash {
                image: dest.clone(),
                expected: self.nar.nar_hash,
                label: self.store_path.clone(),
            })
            .await?;
            Ok(dest)
        })
    }
}

/// Realizes a `builtin:fetchurl` derivation on the host: download, verify
/// its output hash, pack into an EROFS image.
pub struct Fetchurl {
    pub out_path: String,
    pub drv: Derivation,
}

impl Task for Fetchurl {
    type Output = PathBuf;

    fn run(self: Box<Self>, ctx: Context) -> TaskFuture<PathBuf> {
        Box::pin(async move {
            let _permit = ctx.fetch_permit().await?;
            let dest = ctx.image_path(&self.out_path);
            let n = crate::fetchurl::realize_to_image(&self.drv, &dest, &ctx.tmp_dir())
                .await
                .with_context(|| format!("realizing fetchurl for {}", self.out_path))?;
            println!(
                "realized fetchurl {} ({} bytes)",
                nix_drv::basename(&self.out_path),
                n
            );
            Ok(dest)
        })
    }
}
