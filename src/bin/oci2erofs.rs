use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, bail};
use clap::Parser;
use erofs_builder::{CreateOptions, DEFAULT_BLOCK_SIZE, InodeMeta, Writer};
use flate2::read::GzDecoder;
use oci_spec::image::{ImageIndex, ImageManifest, MediaType};
use tar::Archive;
use tokio::fs::File as AsyncFile;
use tokio::io::AsyncRead;
use zstd::Decoder as ZstdDecoder;

#[derive(Parser)]
struct Args {
    /// Path to OCI image layout directory (containing index.json)
    image_dir: PathBuf,
    /// Output erofs image file
    output: PathBuf,
}

enum LayerReader {
    Gzip(GzDecoder<BufReader<File>>),
    Zstd(ZstdDecoder<'static, BufReader<BufReader<File>>>),
}

impl LayerReader {
    fn open(dir: &Path, digest: &str, media_type: &MediaType) -> anyhow::Result<Self> {
        let blob_path = blob_path(dir, digest);
        let file = File::open(&blob_path)
            .with_context(|| format!("opening blob {}", blob_path.display()))?;
        Ok(match media_type {
            MediaType::ImageLayerGzip => LayerReader::Gzip(GzDecoder::new(BufReader::new(file))),
            MediaType::ImageLayerZstd => {
                let br = BufReader::new(file);
                LayerReader::Zstd(ZstdDecoder::new(br)?)
            }
            other => bail!("unsupported layer media type: {other:?}"),
        })
    }
}

impl Read for LayerReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            LayerReader::Gzip(r) => r.read(buf),
            LayerReader::Zstd(r) => r.read(buf),
        }
    }
}

fn blob_path(dir: &Path, digest: &str) -> PathBuf {
    let mut p = dir.join(digest);
    if !p.exists() {
        // OCI layout stores blobs at blobs/<algo>/<hex>; the digest string may
        // be "sha256:<hex>" or just "<hex>"
        let (algo, hex) = match digest.split_once(':') {
            Some((a, h)) => (a, h),
            None => ("sha256", digest),
        };
        p = dir.join("blobs").join(algo).join(hex);
    }
    p
}

// Bridge from sync tar readers into the async writer. Every read is backed by
// a regular file, so the underlying reads block briefly and never return
// WouldBlock.
struct SyncReader<R>(R);

impl<R: Read + Unpin> AsyncRead for SyncReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            match this.0.read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    Reg,
    Sym,
    Hard,
    Char,
    Block,
    Fifo,
}

struct Entry {
    layer: usize,
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    size: u64,
    link: Option<String>,
    dev_major: u32,
    dev_minor: u32,
}

fn normalize(p: &Path) -> PathBuf {
    p.components()
        .filter(|c| {
            !matches!(
                c,
                std::path::Component::CurDir | std::path::Component::RootDir
            )
        })
        .collect()
}

fn meta_for(e: &Entry, perms: u32) -> InodeMeta {
    let ty = match e.kind {
        Kind::Dir => 0o040000,
        Kind::Sym | Kind::Hard => 0o120000,
        Kind::Char => 0o020000,
        Kind::Block => 0o060000,
        Kind::Fifo => 0o010000,
        _ => 0o100000,
    };
    InodeMeta {
        mode: ty | (perms & 0o7777) as u16,
        uid: e.uid,
        gid: e.gid,
        mtime: e.mtime,
        ..Default::default()
    }
}

fn makedev(major: u32, minor: u32) -> u32 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();
    let dir = &args.image_dir;

    let index: ImageIndex =
        serde_json::from_reader(File::open(dir.join("index.json")).context("opening index.json")?)
            .context("parsing index.json")?;
    let manifest_descriptor = index
        .manifests()
        .first()
        .context("no manifests in index.json")?;
    let manifest: ImageManifest = serde_json::from_reader(
        File::open(blob_path(dir, manifest_descriptor.digest().digest()))
            .context("opening manifest blob")?,
    )
    .context("parsing manifest")?;

    // Pass 1 merges all layers into the final tree (later layers win; .wh.*
    // char-device whiteouts delete lower paths). Pass 2 then streams each
    // regular file's data from its winning layer exactly once.
    let mut entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();

    for (layer_idx, layer) in manifest.layers().iter().enumerate() {
        let mut reader = LayerReader::open(dir, layer.digest().digest(), layer.media_type())?;
        let mut archive = Archive::new(&mut reader);

        for entry in archive.entries()? {
            let entry = entry?;
            let raw = entry.path()?.to_path_buf();
            let path = normalize(&raw);
            if path.as_os_str().is_empty() {
                continue;
            }
            let header = entry.header();
            let ft = header.entry_type();

            if ft == tar::EntryType::Char
                && header.device_major()? == Some(0)
                && header.device_minor()? == Some(0)
            {
                let name = raw.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                if let Some(target) = name.strip_prefix(".wh.") {
                    if target == ".wh..wh..opq" {
                        let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
                        entries.retain(|p, _| !p.starts_with(&parent));
                    } else {
                        entries.remove(&path.with_file_name(target));
                    }
                    continue;
                }
            }

            let kind = match ft {
                tar::EntryType::Directory => Kind::Dir,
                tar::EntryType::Regular | tar::EntryType::Continuous => Kind::Reg,
                tar::EntryType::Symlink => Kind::Sym,
                tar::EntryType::Link => Kind::Hard,
                tar::EntryType::Char => Kind::Char,
                tar::EntryType::Block => Kind::Block,
                tar::EntryType::Fifo => Kind::Fifo,
                _ => continue,
            };

            let (dev_major, dev_minor) = match ft {
                tar::EntryType::Char | tar::EntryType::Block => (
                    header.device_major()?.unwrap_or(0),
                    header.device_minor()?.unwrap_or(0),
                ),
                _ => (0, 0),
            };

            entries.insert(
                path,
                Entry {
                    layer: layer_idx,
                    kind,
                    mode: header.mode()? & 0o7777,
                    uid: header.uid()? as u32,
                    gid: header.gid()? as u32,
                    mtime: header.mtime()?.max(0) as u64,
                    size: header.size()?,
                    link: header
                        .link_name()?
                        .map(|l| l.to_string_lossy().into_owned()),
                    dev_major,
                    dev_minor,
                },
            );
        }
    }

    eprintln!("merged entries: {}", entries.len());

    let sink = AsyncFile::create(&args.output).await?;
    // epoch build time keeps the image reproducible and lets mtime-0 files
    // from reproducible layers use compact inodes
    let opts = CreateOptions {
        block_size: DEFAULT_BLOCK_SIZE,
        build_time: 0,
        build_time_nsec: 0,
        ..Default::default()
    };
    let mut fs = Writer::new(sink, opts).await?;

    let mut all_dirs: Vec<_> = entries
        .iter()
        .filter(|(_, e)| e.kind == Kind::Dir)
        .map(|(p, _)| p.clone())
        .collect();
    all_dirs.sort_by_key(|p| (p.components().count(), p.clone()));
    for path in &all_dirs {
        let e = &entries[path];
        let p = path.to_str().context("non-UTF-8 directory path")?;
        fs.mkdir(p, meta_for(e, e.mode))
            .await
            .with_context(|| format!("mkdir {p}"))?;
    }

    let mut files = 0u64;
    for (layer_idx, layer) in manifest.layers().iter().enumerate() {
        let mut reader = LayerReader::open(dir, layer.digest().digest(), layer.media_type())?;
        let mut archive = Archive::new(&mut reader);

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = normalize(&entry.path()?.to_path_buf());
            if path.as_os_str().is_empty() {
                continue;
            }
            let Some(e) = entries.get(&path) else {
                continue;
            };
            if e.layer != layer_idx {
                continue;
            }

            match e.kind {
                Kind::Dir => {}
                Kind::Reg => {
                    let p = path.to_str().context("non-UTF-8 path")?.to_string();
                    fs.add_file(&p, meta_for(e, e.mode), e.size, &mut SyncReader(&mut entry))
                        .await
                        .with_context(|| format!("add_file {p}"))?;
                    files += 1;
                }
                Kind::Sym | Kind::Hard => {
                    let p = path.to_str().context("non-UTF-8 path")?.to_string();
                    let target = e.link.as_deref().context("link without target")?;
                    // the streaming writer has no hardlink support; an
                    // absolute symlink resolves to the same content
                    let target = match e.kind {
                        Kind::Hard => format!("/{}", target.trim_start_matches('/')),
                        _ => target.to_string(),
                    };
                    fs.symlink(&p, target.as_bytes(), meta_for(e, 0o777))
                        .await
                        .with_context(|| format!("symlink {p}"))?;
                }
                Kind::Char | Kind::Block | Kind::Fifo => {
                    let p = path.to_str().context("non-UTF-8 path")?.to_string();
                    let mut meta = meta_for(e, e.mode);
                    if e.kind != Kind::Fifo {
                        meta.rdev = makedev(e.dev_major, e.dev_minor);
                    }
                    fs.mknod(&p, meta)
                        .await
                        .with_context(|| format!("mknod {p}"))?;
                }
            }
        }
    }

    fs.finish().await?;

    let len = std::fs::metadata(&args.output)?.len();
    println!(
        "wrote {} ({} MiB, {} entries, {files} files)",
        args.output.display(),
        len / 1024 / 1024,
        entries.len()
    );
    Ok(())
}
