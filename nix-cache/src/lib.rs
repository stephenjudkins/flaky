//! Nix binary cache client: narinfo lookup and streamed NAR fetching.

use async_compression::tokio::bufread::{GzipDecoder, XzDecoder, ZstdDecoder};
use erofs_builder::Writer;
use futures::TryStreamExt;
use nar_to_erofs::NarDecoder;
use narinfo::NarInfo;
use reqwest::IntoUrl;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, BufReader};
use tokio_util::io::StreamReader;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("narinfo parse: {0}")]
    Narinfo(String),
    #[error("unsupported compression: {0}")]
    UnsupportedCompression(String),
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("invalid store path: {0:?}")]
    InvalidStorePath(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<nar_to_erofs::NarError> for Error {
    fn from(e: nar_to_erofs::NarError) -> Self {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    }
}

/// The 32-character base32 hash part of a nix store path, validated on
/// construction (nix base32 alphabet, no `e`/`o`/`u`/`y`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StorePathHash(String);

const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";

impl StorePathHash {
    pub fn new(s: &str) -> Result<Self> {
        if s.len() != 32 || !s.chars().all(|c| NIX_BASE32.contains(c)) {
            return Err(Error::InvalidStorePath(s.to_string()));
        }
        Ok(StorePathHash(s.to_string()))
    }

    pub fn from_store_path(path: &str) -> Result<Self> {
        let name = path
            .strip_prefix("/nix/store/")
            .ok_or_else(|| Error::InvalidStorePath(path.to_string()))?;
        Self::new(name.split_once('-').map(|(h, _)| h).unwrap_or(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StorePathHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A nix binary cache, e.g. `https://cache.nixos.org`.
#[derive(Clone)]
pub struct NixCache {
    base: reqwest::Url,
    client: reqwest::Client,
}

/// Owned summary of a narinfo hit; just what's needed to stream the NAR.
#[derive(Debug, Clone)]
pub struct CachedNar {
    /// URL of the compressed NAR, relative to the cache root.
    pub url: String,
    pub compression: Compression,
    /// Size of the decompressed NAR in bytes.
    pub nar_size: u64,
    /// Store path names (hash-name, no /nix/store/ prefix) this path
    /// references, from the narinfo `References:` line.
    pub references: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Xz,
    Gzip,
    Zstd,
    None,
}

impl Compression {
    fn from_narinfo(compression: Option<&str>, url: &str) -> Self {
        match compression
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "xz" => Compression::Xz,
            "gzip" | "gz" => Compression::Gzip,
            "zstd" | "zst" => Compression::Zstd,
            "none" | "" if url.ends_with(".nar.xz") => Compression::Xz,
            "none" | "" if url.ends_with(".nar.gz") => Compression::Gzip,
            "none" | "" if url.ends_with(".nar.zst") => Compression::Zstd,
            "none" | "" => Compression::None,
            other => panic!("unsupported compression: {other}"),
        }
    }
}

pub enum Lookup {
    Miss,
    Hit(CachedNar),
}

impl NixCache {
    pub fn new(base: impl IntoUrl) -> Result<Self> {
        let base = base.into_url().map_err(Error::Http)?;
        Ok(NixCache {
            base,
            client: reqwest::Client::new(),
        })
    }

    /// Fetches `<base>/<hash>.narinfo` if present.
    pub async fn lookup(&self, hash: &StorePathHash) -> Result<Lookup> {
        let url = self
            .base
            .join(&format!("{hash}.narinfo"))
            .map_err(|e| Error::Narinfo(e.to_string()))?;
        let resp = self.client.get(url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Lookup::Miss);
        }
        let resp = resp.error_for_status()?;
        let text = resp.text().await?;
        // the narinfo crate rejects unknown keys (e.g. the modern `CA:` line);
        // drop lines it doesn't know before parsing
        const KNOWN: &[&str] = &[
            "StorePath",
            "URL",
            "Compression",
            "FileHash",
            "NarHash",
            "NarSize",
            "FileSize",
            "Deriver",
            "System",
            "References",
            "Sig",
        ];
        let filtered: String = text
            .lines()
            .filter(|l| {
                l.split_once(':')
                    .map(|(k, _)| KNOWN.contains(&k))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let info = NarInfo::parse(&filtered).map_err(|e| Error::Narinfo(format!("{e:?}")))?;
        Ok(Lookup::Hit(CachedNar {
            url: info.url.to_string(),
            compression: Compression::from_narinfo(info.compression.as_deref(), info.url),
            nar_size: info.nar_size as u64,
            references: info
                .references
                .iter()
                .map(|r| r.to_string())
                .filter(|r| !r.is_empty())
                .collect(),
        }))
    }

    /// Opens a decompressed NAR stream for the given cache hit.
    pub async fn nar_stream(&self, nar: &CachedNar) -> Result<Box<dyn AsyncRead + Unpin>> {
        let url = self
            .base
            .join(&nar.url)
            .map_err(|e| Error::Narinfo(e.to_string()))?;
        let resp = self.client.get(url).send().await?.error_for_status()?;
        let byte_stream = resp.bytes_stream().map_err(swallow);
        let raw = StreamReader::new(byte_stream);
        Ok(match nar.compression {
            Compression::Xz => Box::new(XzDecoder::new(BufReader::new(raw))),
            Compression::Gzip => Box::new(GzipDecoder::new(BufReader::new(raw))),
            Compression::Zstd => Box::new(ZstdDecoder::new(BufReader::new(raw))),
            Compression::None => Box::new(BufReader::new(raw)),
        })
    }
}

fn swallow(e: reqwest::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e)
}

/// Streams a NAR from the cache into an EROFS image on `sink`, verifying
/// `nar.nar_size` bytes were consumed. All entries are placed under `prefix`.
pub async fn fetch_nar_to_erofs<W>(
    cache: &NixCache,
    nar: &CachedNar,
    sink: W,
    prefix: Option<&str>,
) -> Result<W>
where
    W: tokio::io::AsyncWrite + tokio::io::AsyncSeek + Unpin + Send,
{
    let stream = cache.nar_stream(nar).await?;
    let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counting = CountingReader {
        inner: stream,
        n: n.clone(),
    };
    let sink = nar_to_erofs::nar_to_image(counting, sink, prefix).await?;
    let produced = n.load(std::sync::atomic::Ordering::SeqCst);
    if produced != nar.nar_size {
        return Err(Error::Narinfo(format!(
            "nar size mismatch: narinfo says {}, stream produced {}",
            nar.nar_size, produced
        )));
    }
    Ok(sink)
}

/// Streams a NAR from the cache into an existing EROFS `Writer` (e.g. a
/// merged store image), placing all entries under `prefix` and verifying
/// `nar.nar_size` bytes were consumed.
pub async fn fetch_nar_into<W>(
    cache: &NixCache,
    nar: &CachedNar,
    writer: &mut Writer<W>,
    prefix: &str,
) -> std::result::Result<(), Error>
where
    W: tokio::io::AsyncWrite + tokio::io::AsyncSeek + Unpin + Send,
{
    let stream = cache.nar_stream(nar).await?;
    let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counting = CountingReader {
        inner: stream,
        n: n.clone(),
    };
    nar_to_erofs::write_nar(NarDecoder::new(counting), writer, Some(prefix)).await?;
    let produced = n.load(std::sync::atomic::Ordering::SeqCst);
    if produced != nar.nar_size {
        return Err(Error::Narinfo(format!(
            "nar size mismatch: narinfo says {}, stream produced {}",
            nar.nar_size, produced
        )));
    }
    Ok(())
}

struct CountingReader<R> {
    inner: R,
    n: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let filled_before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                this.n.fetch_add(
                    (buf.filled().len() - filled_before) as u64,
                    std::sync::atomic::Ordering::SeqCst,
                );
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_hash_validates() {
        assert!(StorePathHash::new("ki4g5pbmqcs07mwrxmvik6dpnzyx3z4c").is_ok());
        assert!(StorePathHash::new("ki4g5pbmqcs07mwrxmvik6dpnzyx3z4e").is_err()); // 'e' not in alphabet
        assert!(StorePathHash::new("short").is_err());
        assert_eq!(
            StorePathHash::from_store_path(
                "/nix/store/846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3"
            )
            .unwrap()
            .as_str(),
            "846h582z2d4mifn4km7axlqllcyn6zdg"
        );
        assert!(StorePathHash::from_store_path("/some/other/path").is_err());
    }

    #[test]
    fn narinfo_parses() {
        let text = "\
StorePath: /nix/store/846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3
URL: nar/846h582z2d4mifn4km7axlqllcyn6zdg.nar.xz
Compression: xz
FileHash: sha256-tQjFHAWTPDNBSqyI2DWfxFOh7MjcUWTOk+frQdpatkc=
FileSize: 196040
NarHash: sha256-F+zLLMvODPI7SRDEKQG7i7KOJc7m0S9HITrFhUiVRNA=
NarSize: 741000
References: abcdefghijklmnopqrstuvwx12345678-name other-name
Deriver: 7pgxjakchwmnbjvkqrym0sqjw29jpgsa-hello-2.12.3.drv
System: aarch64-linux
Sig: cache.nixos.org-1:fake";
        let info = NarInfo::parse(text).map_err(|e| format!("{e:?}")).unwrap();
        assert_eq!(
            info.references,
            vec![
                std::borrow::Cow::from("abcdefghijklmnopqrstuvwx12345678-name"),
                std::borrow::Cow::from("other-name")
            ]
        );
        assert_eq!(info.url, "nar/846h582z2d4mifn4km7axlqllcyn6zdg.nar.xz");
        assert_eq!(
            Compression::from_narinfo(info.compression.as_deref(), info.url),
            Compression::Xz
        );
        assert_eq!(
            Compression::from_narinfo(None, "nar/abc.nar.xz"),
            Compression::Xz
        );
        assert_eq!(
            Compression::from_narinfo(None, "nar/abc.nar"),
            Compression::None
        );
    }

    /// Live test against cache.nixos.org; run with
    /// `cargo test -p nix-cache -- --ignored` when online.
    #[tokio::test]
    #[ignore]
    async fn fetches_nar_from_cache_nixos_org() {
        // /nix/store/wj7phsmi7ncidl8k00p489krqss7n9sd-hello-2.12.3.tar.gz
        let cache = NixCache::new("https://cache.nixos.org").unwrap();
        let hash = StorePathHash::new("wj7phsmi7ncidl8k00p489krqss7n9sd").unwrap();
        let Lookup::Hit(nar) = cache.lookup(&hash).await.unwrap() else {
            panic!("expected cache hit");
        };
        assert!(matches!(nar.compression, Compression::Zstd));
        let sink = std::io::Cursor::new(Vec::new());
        let sink = fetch_nar_to_erofs(
            &cache,
            &nar,
            sink,
            Some("wj7phsmi7ncidl8k00p489krqss7n9sd-hello-2.12.3.tar.gz"),
        )
        .await
        .unwrap();
        let img = sink.into_inner();
        let magic = u32::from_le_bytes([img[1024], img[1025], img[1026], img[1027]]);
        assert_eq!(magic, 0xE0F5E1E2, "erofs magic");
        assert!(img.len() > 1024 + 128, "image has content");
    }
}
