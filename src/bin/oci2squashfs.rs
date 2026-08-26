use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use backhand::{FilesystemCompressor, FilesystemWriter, NodeHeader, kind};
use clap::Parser;
use flate2::read::GzDecoder;
use oci_spec::image::{ImageIndex, ImageManifest, MediaType};
use tar::Archive;
use zstd::Decoder as ZstdDecoder;

#[derive(Parser)]
struct Args {
    /// Path to OCI image layout directory (containing index.json)
    image_dir: PathBuf,
    /// Output squashfs file
    output: PathBuf,
}

enum LayerReader {
    Gzip(GzDecoder<BufReader<File>>),
    Zstd(ZstdDecoder<'static, BufReader<BufReader<File>>>),
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

impl LayerReader {
    fn open(dir: &Path, digest: &str, media_type: &MediaType) -> anyhow::Result<Self> {
        let blob_path = blob_path(dir, digest);
        let file = File::open(&blob_path).map_err(|e| {
            eprintln!("failed to open blob: {}", blob_path.display());
            e
        })?;
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

struct Entry {
    layer: usize,
    is_dir: bool,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    link: Option<String>,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();
    let dir = &args.image_dir;

    let index: ImageIndex = serde_json::from_reader(
        File::open(dir.join("index.json")).context("opening index.json")?,
    )
    .context("parsing index.json")?;
    let manifest_descriptor = index
        .manifests()
        .first()
        .context("no manifests in index.json")?;
    let manifest_path = blob_path(dir, manifest_descriptor.digest().digest());
    eprintln!("manifest path: {}", manifest_path.display());
    let manifest: ImageManifest = serde_json::from_reader(
        File::open(&manifest_path).context("opening manifest blob")?,
    )
    .context("parsing manifest")?;

    // Overlay semantics: later layers win for duplicate paths; whiteouts
    // (char device 0/0 named .wh.*) delete lower-layer paths.
    //
    // Pass 1 walks every layer and builds the final merged tree metadata
    // only (no file data). Pass 2 then walks layers in order once each;
    // for every regular file whose winning layer is the current one, its
    // data is streamed straight into the writer. Nodes are emitted grouped
    // by layer, so parent dirs are pushed (with default header) on first
    // touch and their real metadata is patched afterwards via mut_file.
    let mut entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();

    for (layer_idx, layer) in manifest.layers().iter().enumerate() {
        let mut reader =
            LayerReader::open(dir, layer.digest().digest(), layer.media_type())?;
        let mut archive = Archive::new(&mut reader);

        for entry in archive.entries()? {
            let entry = entry?;
            let path = entry.path()?.into_owned();
            // normalize: strip leading '/' and './', drop trailing slashes
            let path: PathBuf = path
                .components()
                .filter(|c| !matches!(c, std::path::Component::CurDir | std::path::Component::RootDir))
                .collect();
            if path.as_os_str().is_empty() {
                continue;
            }
            let header = entry.header();
            let ft = header.entry_type();

            if ft == tar::EntryType::Char
                && header.device_major()? == Some(0)
                && header.device_minor()? == Some(0)
            {
                let name = entry
                    .path()?
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if let Some(target) = name.strip_prefix(".wh.") {
                    if target == ".wh..wh..opq" {
                        let parent = entry.path()?.parent().unwrap_or(Path::new("")).to_path_buf();
                        entries.retain(|p, _| !p.starts_with(&parent));
                    } else {
                        entries.remove(&entry.path()?.with_file_name(target));
                    }
                    continue;
                }
            }

            entries.insert(
                path,
                Entry {
                    layer: layer_idx,
                    is_dir: ft == tar::EntryType::Directory,
                    mode: header.mode()? & 0o7777,
                    uid: header.uid()? as u32,
                    gid: header.gid()? as u32,
                    mtime: header.mtime()?.max(0) as u64,
                    link: header.link_name()?.map(|l| l.to_string_lossy().into_owned()),
                },
            );
        }
    }

    eprintln!("merged entries: {}", entries.len());
    let mut fs = FilesystemWriter::default();
    fs.set_kind(kind::Kind::from_const(kind::LE_V4_0).unwrap());
    fs.set_root_mode(0o755);
    let mut compressor = FilesystemCompressor::new(
        backhand::compression::Compressor::Zstd,
        Some(backhand::compression::CompressionOptions::Zstd(
            backhand::v4::compressor::Zstd {
                compression_level: 3,
            },
        )),
    )?;
    fs.set_compressor(compressor);

    // Push ALL directories first (sorted by depth, parents before children)
    // with their final merged metadata. This avoids ordering problems where
    // a dir's winning layer comes after files that need it as a parent.
    let mut pushed_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut all_dirs: Vec<_> = entries
        .iter()
        .filter(|(_, e)| e.is_dir)
        .map(|(p, _)| p.clone())
        .collect();
    all_dirs.sort_by_key(|p| (p.components().count(), p.clone()));
    for path in &all_dirs {
        let e = &entries[path];
        let header = NodeHeader {
            permissions: e.mode as u16,
            uid: e.uid,
            gid: e.gid,
            mtime: e.mtime as u32,
        };
        fs.push_dir(path, header).map_err(|e| {
            anyhow::anyhow!("push_dir {}: {e}", path.display())
        })?;
        pushed_dirs.insert(path.clone());
    }

    // Pass 2: one sequential decompress per layer, in order. A file's data
    // is only taken from its winning layer, so overlay correctness holds.
    fn process_layer(
        fs: &mut FilesystemWriter,
        entries: &BTreeMap<PathBuf, Entry>,
        dir: &Path,
        layer_idx: usize,
        layer: &oci_spec::image::Descriptor,
    ) -> anyhow::Result<()> {
        let mut reader = LayerReader::open(dir, layer.digest().digest(), layer.media_type())?;
        let mut archive = Archive::new(&mut reader);

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path: PathBuf = entry
                .path()?
                .components()
                .filter(|c| !matches!(
                    c,
                    std::path::Component::CurDir | std::path::Component::RootDir
                ))
                .collect();
            if path.as_os_str().is_empty() {
                continue;
            }
            let Some(e) = entries.get(&path) else {
                continue; // shadowed by a later layer or whited out
            };
            if e.layer != layer_idx {
                continue; // not the winning layer for this path
            }

            if e.is_dir {
                continue; // dirs already pushed up-front
            }

            let header = NodeHeader {
                permissions: e.mode as u16,
                uid: e.uid,
                gid: e.gid,
                mtime: e.mtime as u32,
            };

            if let Some(link) = &e.link {
                fs.push_symlink(link.clone(), &path, header)?;
                continue;
            }

            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            fs.push_file(Cursor::new(data), &path, header)
                .map_err(|e| anyhow::anyhow!("push_file {}: {e}", path.display()))?;
        }
        Ok(())
    }

    for (layer_idx, layer) in manifest.layers().iter().enumerate() {
        process_layer(&mut fs, &entries, dir, layer_idx, layer)?;
    }

    let out = File::create(&args.output)?;
    fs.write(out)?;
    println!("wrote {} ({} entries)", args.output.display(), entries.len());
    Ok(())
}
