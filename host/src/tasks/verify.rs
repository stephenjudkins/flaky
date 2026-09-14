//! Verifying the NAR hash of a finished EROFS image by walking it with
//! the erofs reader and hashing the tree in NAR order.

use std::path::PathBuf;

use anyhow::Context as _;

use crate::task::{Context, Task, TaskFuture};

pub struct VerifyNarHash {
    pub image: PathBuf,
    pub expected: [u8; 32],
    /// What to call the image in error messages.
    pub label: String,
}

impl Task for VerifyNarHash {
    type Output = ();

    fn run(self: Box<Self>, _ctx: Context) -> TaskFuture<()> {
        Box::pin(async move {
            let digest = crate::tarball::image_nar_digest(&self.image)
                .await
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("hashing image {}", self.image.display()))?;
            anyhow::ensure!(
                digest == self.expected,
                "nar hash mismatch for {}: expected {}, got {}",
                self.label,
                nix_drv::nix_base32_encode(&self.expected),
                nix_drv::nix_base32_encode(&digest),
            );
            Ok(())
        })
    }
}
