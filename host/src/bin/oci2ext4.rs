use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use arcbox_ext4::Formatter;
use clap::Parser;
use flate2::read::GzDecoder;
use oci_spec::image::{ImageIndex, ImageManifest, MediaType};
use zstd::Decoder as ZstdDecoder;

#[derive(Parser)]
struct Args {
    /// Path to OCI image layout directory (containing index.json)
    image_dir: PathBuf,
    /// Output ext4 image file
    output: PathBuf,
    /// Initial image size in MiB; grows to fit content (group-aligned).
    /// [default: 64]
    #[arg(long, default_value_t = 64)]
    size_mib: u64,
}

enum LayerReader {
    Gzip(GzDecoder<BufReader<File>>),
    Zstd(ZstdDecoder<'static, BufReader<BufReader<File>>>),
}

impl LayerReader {
    fn open(dir: &Path, digest: &str, media_type: &MediaType) -> anyhow::Result<Self> {
        let blob_path = blob_path(dir, digest);
        let file =
            File::open(&blob_path).with_context(|| format!("opening blob {}", blob_path.display()))?;
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
    let manifest: ImageManifest = serde_json::from_reader(
        File::open(blob_path(dir, manifest_descriptor.digest().digest()))
            .context("opening manifest blob")?,
    )
    .context("parsing manifest")?;

    // Single pass: layers are applied in order; arcbox's unpack_tar handles
    // overlay semantics (whiteouts, hardlinks) natively.
    let size_bytes = args.size_mib * 1024 * 1024;
    let mut fmt = Formatter::new(&args.output, 4096, size_bytes)
        .map_err(|e| anyhow::anyhow!("creating ext4 formatter: {e}"))?;

    for layer in manifest.layers() {
        let mut reader = LayerReader::open(dir, layer.digest().digest(), layer.media_type())?;
        fmt.unpack_tar(&mut reader)
            .map_err(|e| anyhow::anyhow!("unpacking layer {}: {e}", layer.digest().digest()))?;
    }

    fmt.close()
        .map_err(|e| anyhow::anyhow!("finalizing ext4 image: {e}"))?;

    shrink_to_minimum(&args.output)?;
    let mib = std::fs::metadata(&args.output)?.len() / 1024 / 1024;
    println!("wrote {} ({} MiB)", args.output.display(), mib);
    Ok(())
}

// The formatter computes the group count from content, not from the
// requested size, so everything past blocks_count*block_size is zero
// padding when a generous size was requested. Truncate to the fs's own
// declared extent.
fn shrink_to_minimum(path: &Path) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom};

    const SB: u64 = 1024; // superblock offset
    const BS: u64 = 4096;

    let mut f = File::options().read(true).write(true).open(path)?;
    let mut sb = [0u8; 16];
    f.seek(SeekFrom::Start(SB))?;
    f.read_exact(&mut sb)?;
    let total_blocks = u32::from_le_bytes(sb[4..8].try_into()?) as u64;
    eprintln!(
        "shrink check: file={}MiB total_blocks={total_blocks} fs_len={}MiB",
        f.metadata()?.len() / 1024 / 1024,
        total_blocks * BS / 1024 / 1024
    );
    let fs_len = total_blocks * BS;
    let len = f.metadata()?.len();
    if fs_len > 0 && fs_len < len {
        f.set_len(fs_len)?;
        eprintln!(
            "shrunk image: {} -> {} MiB",
            len / 1024 / 1024,
            fs_len / 1024 / 1024
        );
    }
    Ok(())
}
