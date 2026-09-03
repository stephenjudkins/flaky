//! Nix binary cache client: narinfo lookup and streamed NAR fetching.

use async_compression::tokio::bufread::{GzipDecoder, XzDecoder, ZstdDecoder};
use erofs_builder::Writer;
use futures::TryStreamExt;
use nar_to_erofs::NarDecoder;
use narinfo::NarInfo;
use reqwest::IntoUrl;
use std::path::PathBuf;
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
///
/// When configured with [`NixCache::with_disk_cache`], narinfo hits are
/// memoized on disk forever: a narinfo is immutable, content-addressed
/// data. Misses are not recorded — lookups only happen for paths being
/// materialized, which is exactly when a fresh answer is wanted (the path
/// may have appeared upstream since).
#[derive(Clone)]
pub struct NixCache {
    base: reqwest::Url,
    client: reqwest::Client,
    disk: Option<PathBuf>,
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
            disk: None,
        })
    }

    /// Memoizes narinfo lookups under `root/<cache-host>/<hash>.narinfo`.
    pub fn with_disk_cache(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        self.disk = Some(root.join(host_key(&self.base)));
        self
    }

    /// Disk-only lookup: returns a hit previously memoized by `lookup`,
    /// without touching the network. Legacy negative-marker files are
    /// removed on sight.
    pub fn lookup_local(&self, hash: &StorePathHash) -> Result<Option<Lookup>> {
        let Some(dir) = &self.disk else {
            return Ok(None);
        };
        let path = dir.join(format!("{hash}.narinfo"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        if text.starts_with("miss") {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        match parse_narinfo(&text) {
            Ok(nar) => Ok(Some(Lookup::Hit(nar))),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                eprintln!("warning: discarding corrupt cached narinfo {path:?}: {e}");
                Ok(None)
            }
        }
    }

    fn write_local(&self, hash: &StorePathHash, contents: &str) {
        let Some(dir) = &self.disk else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let _ = std::fs::write(dir.join(format!("{hash}.narinfo")), contents);
    }

    /// Fetches `<base>/<hash>.narinfo`, consulting the local disk cache
    /// first when configured.
    pub async fn lookup(&self, hash: &StorePathHash) -> Result<Lookup> {
        if let Some(found) = self.lookup_local(hash)? {
            return Ok(found);
        }
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
        let nar = parse_narinfo(&text)?;
        self.write_local(hash, &text);
        Ok(Lookup::Hit(nar))
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

fn host_key(base: &reqwest::Url) -> String {
    let mut s = base.host_str().unwrap_or("cache").to_string();
    if let Some(port) = base.port() {
        s.push_str(&format!("-{port}"));
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn parse_narinfo(text: &str) -> Result<CachedNar> {
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
    Ok(CachedNar {
        url: info.url.to_string(),
        compression: Compression::from_narinfo(info.compression.as_deref(), info.url),
        nar_size: info.nar_size as u64,
        references: info
            .references
            .iter()
            .map(|r| r.to_string())
            .filter(|r| !r.is_empty())
            .collect(),
    })
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
    let mut decoder = NarDecoder::new(counting);
    nar_to_erofs::write_nar(&mut decoder, writer, Some(prefix)).await?;
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
mod tests;
