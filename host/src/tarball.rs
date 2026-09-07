//! Packing a github tarball into an erofs image, without unpacking it to a
//! tree on disk and without buffering it anywhere: entry data streams
//! straight from the tar into the image in arrival order.
//!
//! The NAR hash is derived afterwards from the finished image itself: the
//! erofs reader walks the tree in NAR order (depth-first, children sorted
//! by name bytes) and hashes what is actually stored. That order cannot
//! be produced while streaming: git lists directories as `dir/`, so a
//! directory can arrive after siblings that byte-sort after it (nixpkgs
//! has thousands of these), and sha256 over the NAR stream is not
//! decomposable into per-file hashes.
//!
//! Anything structurally unexpected is reported as an error string so the
//! caller can fall back to the unpack-to-disk path.

use std::path::Path;

use erofs_builder::InodeMeta;
use futures::StreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncRead;

pub(crate) type ImageWriter = erofs_builder::Writer<nar_to_erofs::CountingSink<tokio::fs::File>>;

struct NarHasher(Sha256);

impl NarHasher {
    fn new() -> Self {
        NarHasher(Sha256::new())
    }

    fn tok(&mut self, s: &[u8]) {
        self.0.update((s.len() as u64).to_le_bytes());
        self.0.update(s);
        let pad = (8 - s.len() % 8) % 8;
        self.0.update(&[0u8; 8][..pad]);
    }

    fn begin(&mut self) {
        self.tok(b"nix-archive-1");
        self.tok(b"(");
        self.tok(b"type");
        self.tok(b"directory");
    }

    fn entry(&mut self, name: &[u8]) {
        self.tok(b"entry");
        self.tok(b"(");
        self.tok(b"name");
        self.tok(name);
        self.tok(b"node");
    }

    fn dir_open(&mut self) {
        self.tok(b"(");
        self.tok(b"type");
        self.tok(b"directory");
    }

    fn dir_close(&mut self) {
        self.tok(b")");
        self.tok(b")");
    }

    fn file_open(&mut self, name: &[u8], executable: bool, size: u64) {
        self.entry(name);
        self.tok(b"(");
        self.tok(b"type");
        self.tok(b"regular");
        if executable {
            self.tok(b"executable");
            self.tok(b"");
        }
        self.tok(b"contents");
        self.0.update(size.to_le_bytes());
    }

    fn data(&mut self, buf: &[u8]) {
        self.0.update(buf);
    }

    fn file_close(&mut self, size: u64) {
        let pad = ((8 - size % 8) % 8) as usize;
        self.0.update(&[0u8; 8][..pad]);
        self.tok(b")");
        self.tok(b")");
    }

    fn symlink(&mut self, name: &[u8], target: &[u8]) {
        self.entry(name);
        self.tok(b"(");
        self.tok(b"type");
        self.tok(b"symlink");
        self.tok(b"target");
        self.tok(target);
        self.tok(b")");
        self.tok(b")");
    }

    fn finish(mut self) -> [u8; 32] {
        self.tok(b")");
        self.0.finalize().into()
    }
}

/// Reads `tar` (an uncompressed tar stream whose entries live under the
/// directory `top`), streaming it into a fresh erofs image created at
/// `img_path` under `base`. The error string describes why streaming is
/// not possible; callers fall back to the disk-based path. The NAR hash
/// of the tree is computed separately, after the image is finished, with
/// [`image_nar_digest`].
pub(crate) async fn tar_to_image<R>(
    tar: R,
    top: &str,
    base: &str,
    volume: &str,
    img_path: &Path,
) -> Result<ImageWriter, String>
where
    R: AsyncRead + Unpin + Send,
{
    let file = tokio::fs::File::create(img_path)
        .await
        .map_err(|e| format!("image: {e}"))?;
    let mut writer = nar_to_erofs::image_writer(file, volume)
        .await
        .map_err(|e| format!("erofs: {e}"))?;

    // match write_nar: the store-path prefix dir is created explicitly
    // with dir(0o555), not implicitly with default metadata
    writer
        .mkdir(base, InodeMeta::dir(0o555))
        .await
        .map_err(|e| format!("erofs: {e}"))?;

    let mut archive = tokio_tar::Archive::new(tar);
    let mut entries = archive.entries().map_err(|e| e.to_string())?;

    let prefix = format!("{top}/");
    let mut saw_root = false;
    while let Some(entry) = entries.next().await {
        let mut entry = entry.map_err(|e| format!("tar: {e}"))?;
        let path = entry.path_bytes();
        let path = match std::str::from_utf8(&path) {
            Ok(p) => p.to_string(),
            Err(_) => return Err(format!("non-utf8 entry path {:?}", path)),
        };
        if entry.header().entry_type() == tokio_tar::EntryType::XGlobalHeader {
            continue;
        }
        if !saw_root {
            if path != top && path != prefix {
                return Err(format!("first tar entry {path} is not {top}"));
            }
            if !entry.header().entry_type().is_dir() {
                return Err(format!("first tar entry {path} is not a directory"));
            }
            saw_root = true;
            continue;
        }
        let rel = path
            .strip_prefix(&prefix)
            .ok_or_else(|| format!("entry {path} is outside {top}"))?;
        let rel = rel.strip_suffix('/').unwrap_or(rel);
        if rel.is_empty()
            || rel
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(format!("bad entry path {path}"));
        }
        let t = entry.header().entry_type();
        let dst = format!("{base}/{rel}");
        if t.is_dir() {
            writer
                .mkdir(&dst, InodeMeta::dir(0o555))
                .await
                .map_err(|err| format!("erofs: {err}"))?;
        } else if t.is_symlink() {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| format!("symlink {path} has no target"))?;
            let target = target.into_owned();
            writer
                .symlink(&dst, &target, InodeMeta::symlink())
                .await
                .map_err(|err| format!("erofs: {err}"))?;
        } else if t.is_file() {
            let mode = entry.header().mode().unwrap_or(0);
            let size = entry.header().size().unwrap_or(0);
            let meta = InodeMeta::reg(if mode & 0o111 != 0 { 0o555 } else { 0o444 });
            writer
                .add_file(&dst, meta, size, &mut entry)
                .await
                .map_err(|err| format!("erofs: {err}"))?;
        } else {
            return Err(format!("unsupported tar entry type for {path}"));
        }
    }
    Ok(writer)
}

/// Computes the NAR digest of the single tree mounted under the image
/// root, by walking the finished image with the erofs reader.
pub(crate) async fn image_nar_digest(img_path: &Path) -> Result<[u8; 32], String> {
    let mut reader = erofs_builder::Reader::open(img_path)
        .await
        .map_err(|e| format!("erofs reader: {e}"))?;
    let mut roots = reader
        .dirents(reader.root_nid())
        .await
        .map_err(|e| format!("erofs reader: {e}"))?;
    let [root] = roots.as_mut_slice() else {
        return Err("image root must contain exactly one entry".into());
    };
    if root.kind != erofs_builder::FileType::Dir {
        return Err("image root entry is not a directory".into());
    }
    let base_nid = root.nid;

    let mut hasher = NarHasher::new();
    hasher.begin();
    let mut stack: Vec<(Vec<erofs_builder::Dirent>, usize)> =
        vec![(sorted_dirents(&mut reader, base_nid).await?, 0)];
    while let Some(frame) = stack.last_mut() {
        if frame.1 >= frame.0.len() {
            stack.pop();
            if !stack.is_empty() {
                hasher.dir_close();
            }
            continue;
        }
        let (name, nid) = {
            let d = &frame.0[frame.1];
            frame.1 += 1;
            (d.name.clone(), d.nid)
        };
        let stat = reader
            .stat(nid)
            .await
            .map_err(|e| format!("erofs reader: {e}"))?;
        match stat.kind {
            erofs_builder::FileType::Dir => {
                hasher.entry(name.as_bytes());
                hasher.dir_open();
                stack.push((sorted_dirents(&mut reader, nid).await?, 0));
            }
            erofs_builder::FileType::Symlink => {
                let mut target = Vec::with_capacity(stat.size as usize);
                reader
                    .read_content(nid, &mut |chunk| target.extend_from_slice(chunk))
                    .await
                    .map_err(|e| format!("erofs reader: {e}"))?;
                hasher.symlink(name.as_bytes(), &target);
            }
            erofs_builder::FileType::RegFile => {
                hasher.file_open(name.as_bytes(), stat.mode & 0o111 != 0, stat.size);
                reader
                    .read_content(nid, &mut |chunk| hasher.data(chunk))
                    .await
                    .map_err(|e| format!("erofs reader: {e}"))?;
                hasher.file_close(stat.size);
            }
            _ => return Err(format!("unsupported file type for {name}")),
        }
    }
    Ok(hasher.finish())
}

async fn sorted_dirents(
    reader: &mut erofs_builder::Reader,
    nid: u64,
) -> Result<Vec<erofs_builder::Dirent>, String> {
    let mut entries = reader
        .dirents(nid)
        .await
        .map_err(|e| format!("erofs reader: {e}"))?;
    // the writer stores dirents name-sorted; sort defensively anyway
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

#[cfg(test)]
mod tests;
