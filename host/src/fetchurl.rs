//! Host-side realization of `builtin:fetchurl` derivations: download,
//! verify the output hash, and pack the file into its own EROFS image
//! (volume name = hash[..16]) as a synthesized single-file NAR.

use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use futures::TryStreamExt;
use nix_drv::Derivation;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

fn nar_token(s: &str) -> Vec<u8> {
    let mut v = (s.len() as u64).to_le_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v.extend(std::iter::repeat_n(0, (8 - (s.len() % 8)) % 8));
    v
}

fn nar_prefix(size: u64, executable: bool) -> Vec<u8> {
    let mut v = nar_token("nix-archive-1");
    v.extend(nar_token("("));
    v.extend(nar_token("type"));
    v.extend(nar_token("regular"));
    if executable {
        v.extend(nar_token("executable"));
        v.extend(nar_token(""));
    }
    v.extend(nar_token("contents"));
    v.extend(size.to_le_bytes());
    v
}

fn nar_suffix() -> Vec<u8> {
    nar_token(")")
}

fn pad(n: u64) -> u64 {
    (8 - (n % 8)) % 8
}

fn expected_digest(drv: &Derivation) -> anyhow::Result<[u8; 32]> {
    let h = nix_drv::env_value(drv, "outputHash")
        .ok_or_else(|| anyhow!("fetchurl drv without outputHash"))?;
    if let Some(b64) = h.strip_prefix("sha256-") {
        let v = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .with_context(|| format!("decoding outputHash {h}"))?;
        let d: [u8; 32] = v
            .try_into()
            .map_err(|_| anyhow!("sha256 digest must be 32 bytes"))?;
        Ok(d)
    } else {
        bail!("unsupported outputHash format {h} (need sha256-<base64>)")
    }
}

async fn download(url: &str, dest: &Path) -> anyhow::Result<u64> {
    let resp = reqwest::get(url).await?;
    let resp = resp.error_for_status()?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream().map_err(|e| std::io::Error::other(e));
    let mut total = 0u64;
    while let Some(chunk) = stream.try_next().await? {
        file.write_all(chunk.as_ref()).await?;
        total += chunk.len() as u64;
    }
    file.flush().await?;
    Ok(total)
}

async fn digest_of_file(
    path: &Path,
    mode: &str,
    size: u64,
    executable: bool,
) -> std::io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    if mode == "recursive" {
        hasher.update(nar_prefix(size, executable));
    }
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; 1 << 16];
    let mut left = size;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..n]).await?;
        if n == 0 {
            return Err(std::io::Error::other("file truncated while hashing"));
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    if mode == "recursive" {
        let zeros = vec![0u8; pad(size) as usize];
        hasher.update(&zeros);
        hasher.update(nar_suffix());
    }
    Ok(hasher.finalize().into())
}

/// Chain of prefix bytes + file + suffix as one AsyncRead.
struct NarOfFile {
    file: tokio::fs::File,
    prefix: std::io::Cursor<Vec<u8>>,
    suffix: std::io::Cursor<Vec<u8>>,
    size: u64,
    read: u64,
    padded: bool,
    state: u8,
}

impl AsyncRead for NarOfFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            match this.state {
                0 => {
                    let mut tmp = tokio::io::ReadBuf::new(buf.initialize_unfilled());
                    match Pin::new(&mut this.prefix).poll_read(cx, &mut tmp) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => {
                            let n = tmp.filled().len();
                            buf.advance(n);
                            if n == 0 {
                                this.state = 1;
                                continue;
                            }
                            return Poll::Ready(Ok(()));
                        }
                        pending => return pending,
                    }
                }
                1 => {
                    if this.read < this.size {
                        let want =
                            ((this.size - this.read).min(buf.remaining() as u64) as usize).max(1);
                        let mut tmp = tokio::io::ReadBuf::new(buf.initialize_unfilled_to(want));
                        match Pin::new(&mut this.file).poll_read(cx, &mut tmp) {
                            Poll::Ready(Ok(())) => {
                                let n = tmp.filled().len();
                                if n == 0 {
                                    return Poll::Ready(Err(std::io::Error::other(
                                        "nar file truncated",
                                    )));
                                }
                                this.read += n as u64;
                                buf.advance(n);
                                return Poll::Ready(Ok(()));
                            }
                            other => return other,
                        }
                    } else if !this.padded {
                        this.padded = true;
                        let n = pad(this.size).min(buf.remaining() as u64) as usize;
                        let zeroes = vec![0u8; n];
                        buf.put_slice(&zeroes);
                        if n == 0 {
                            continue;
                        }
                        return Poll::Ready(Ok(()));
                    } else {
                        this.state = 2;
                        continue;
                    }
                }
                _ => {
                    let mut tmp = tokio::io::ReadBuf::new(buf.initialize_unfilled());
                    match Pin::new(&mut this.suffix).poll_read(cx, &mut tmp) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => {
                            let n = tmp.filled().len();
                            buf.advance(n);
                            if n == 0 {
                                return Poll::Ready(Ok(()));
                            }
                            return Poll::Ready(Ok(()));
                        }
                        pending => return pending,
                    }
                }
            }
        }
    }
}

use std::pin::Pin;
use std::task::Poll;

/// Realizes the fetchurl drv into a fresh EROFS image at `dest`.
/// Returns the image size in bytes.
pub async fn realize_to_image(
    drv: &Derivation,
    dest: &Path,
    scratch_dir: &Path,
) -> anyhow::Result<u64> {
    let out_path =
        nix_drv::env_value(drv, "out").ok_or_else(|| anyhow!("fetchurl drv without out"))?;
    let base = nix_drv::basename(&out_path).to_string();
    let mode = nix_drv::env_value(drv, "outputHashMode").unwrap_or_else(|| "flat".to_string());
    let executable = nix_drv::env_value(drv, "executable").as_deref() == Some("1");
    let expected = expected_digest(drv)?;
    let urls: Vec<String> = nix_drv::env_value(drv, "urls")
        .or_else(|| nix_drv::env_value(drv, "url"))
        .ok_or_else(|| anyhow!("fetchurl drv without url"))?
        .split_whitespace()
        .map(String::from)
        .collect();

    std::fs::create_dir_all(scratch_dir)?;
    let tmp = scratch_dir.join(&base);

    let mut size = None;
    let mut last_err = None;
    for u in &urls {
        eprintln!("fetchurl: {u}");
        match download(u, &tmp).await {
            Ok(n) => {
                size = Some(n);
                break;
            }
            Err(e) => {
                eprintln!("fetchurl: {u} failed: {e:#}");
                last_err = Some(e);
            }
        }
    }
    let size = match size {
        Some(n) => n,
        None => return Err(last_err.unwrap()),
    };

    let digest = digest_of_file(&tmp, &mode, size, executable)
        .await
        .context("hashing fetchurl output")?;
    if digest != expected {
        bail!(
            "hash mismatch for {out_path}: expected {}, got {}",
            nix_drv::nix_base32_encode(&expected),
            nix_drv::nix_base32_encode(&digest)
        );
    }
    eprintln!(
        "fetchurl: verified {out_path} ({} bytes, {mode} sha256 ok)",
        size
    );

    let file = tokio::fs::File::create(dest).await?;
    let mut writer = nar_to_erofs::image_writer(file, &apis::volume_id(&out_path)).await?;

    let reader = NarOfFile {
        file: tokio::fs::File::open(&tmp).await?,
        prefix: std::io::Cursor::new(nar_prefix(size, executable)),
        suffix: std::io::Cursor::new(nar_suffix()),
        size,
        read: 0,
        padded: false,
        state: 0,
    };
    let mut nar = nar_to_erofs::NarDecoder::new(reader);
    nar_to_erofs::write_nar(&mut nar, &mut writer, Some(&base))
        .await
        .map_err(|e| anyhow!("writing fetchurl nar: {e}"))?;
    let (file, image_size) = nar_to_erofs::finish_image(writer).await?;
    file.set_len(image_size).await?;
    file.sync_all().await?;
    let _ = std::fs::remove_file(&tmp);
    Ok(image_size)
}

#[cfg(test)]
mod tests;
