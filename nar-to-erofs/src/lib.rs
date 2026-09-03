//! Streaming NAR (Nix ARchive) decoder and EROFS image writer.
//!
//! The decoder consumes a NAR byte stream from any `AsyncRead` and yields
//! typed filesystem entries in depth-first order. File contents are exposed
//! through a dedicated reader so that large files can be streamed straight
//! into an EROFS image without buffering.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use erofs_builder::{CreateOptions, DEFAULT_BLOCK_SIZE, InodeMeta, Writer};
use tokio::io::SeekFrom;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncWrite};

/// An entry emitted by [`NarDecoder`] in depth-first pre-order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the NAR root, using `/` separators, no leading `/`.
    pub path: String,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    Regular {
        executable: bool,
        /// Content length in bytes. Read the bytes via [`NarDecoder::data`].
        size: u64,
    },
    Symlink {
        target: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum NarError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("bad magic: expected \"nix-archive-1\"")]
    BadMagic,
    #[error("expected token {expected:?}, got {got:?}")]
    UnexpectedToken { expected: &'static str, got: String },
    #[error("unexpected end of archive")]
    Eof,
    #[error("entry name {0:?} is not strictly after previous entry in the same directory")]
    UnsortedEntries(String),
    #[error("invalid entry name {0:?}")]
    InvalidName(String),
    #[error("token or name exceeds size limit ({limit} bytes)")]
    TokenTooLarge { limit: usize },
    #[error("regular file at NAR root; a prefix is required to place it in the image")]
    RootFileNeedsPrefix,
}

pub type Result<T, E = NarError> = std::result::Result<T, E>;

enum State {
    Start,
    /// At the `type` token of a node; `wrapper` is true when the node is
    /// wrapped in an `entry ( name .. node .. )` form.
    ExpectNode {
        wrapper: bool,
    },
    /// Inside a directory body, expecting `entry` or `)`.
    InDirectory,
    /// Just emitted a file/symlink node; must skip remaining data and
    /// padding, then consume `closes` `)` tokens.
    CloseNode {
        closes: u32,
    },
    Finished,
}

struct DirFrame {
    /// This directory's own name within its parent (empty for the root).
    name: String,
    /// Name of the previous entry in this directory, for sortedness checks.
    last_name: Vec<u8>,
}

const MAX_TOKEN: u64 = 64 << 10;

/// Streaming NAR decoder. Entries are yielded by [`NarDecoder::next_entry`];
/// after a `Regular` entry the content bytes are read via [`NarDecoder::data`].
pub struct NarDecoder<R> {
    src: R,
    state: State,
    frames: Vec<DirFrame>,
    /// Name of the entry currently being descended into (set while parsing
    /// `entry ( name <name> node ...`).
    pending: Vec<u8>,
    /// Remaining unread bytes of the current file's contents.
    data_remaining: u64,
    /// Total size of the current file's contents (for padding).
    data_total: u64,
    scratch: Vec<u8>,
}

fn pad(n: u64) -> u64 {
    (8 - (n % 8)) % 8
}

impl<R: AsyncRead + Unpin> NarDecoder<R> {
    pub fn new(src: R) -> Self {
        NarDecoder {
            src,
            state: State::Start,
            frames: Vec::new(),
            pending: Vec::new(),
            data_remaining: 0,
            data_total: 0,
            scratch: Vec::new(),
        }
    }

    pub fn into_inner(self) -> R {
        self.src
    }

    /// Returns the next entry, or `None` at end of archive.
    pub async fn next_entry(&mut self) -> Result<Option<Entry>> {
        loop {
            match std::mem::replace(&mut self.state, State::Finished) {
                State::Start => {
                    let magic = self.read_string().await?;
                    if magic != b"nix-archive-1" {
                        return Err(NarError::BadMagic);
                    }
                    self.state = State::ExpectNode { wrapper: false };
                }
                State::ExpectNode { wrapper } => {
                    self.expect_token("(").await?;
                    self.expect_token("type").await?;
                    let ty = self.read_string().await?;
                    match ty.as_slice() {
                        b"regular" => {
                            let mut executable = false;
                            loop {
                                let tok = self.read_string().await?;
                                match tok.as_slice() {
                                    b"executable" => {
                                        self.expect_token("").await?;
                                        executable = true;
                                    }
                                    b"contents" => break,
                                    other => {
                                        return Err(NarError::UnexpectedToken {
                                            expected: "executable|contents",
                                            got: String::from_utf8_lossy(other).into_owned(),
                                        });
                                    }
                                }
                            }
                            let len = self.read_u64().await?;
                            self.data_total = len;
                            self.data_remaining = len;
                            self.state = State::CloseNode {
                                closes: u32::from(wrapper) + 1,
                            };
                            return Ok(Some(Entry {
                                path: self.current_path()?,
                                kind: EntryKind::Regular {
                                    executable,
                                    size: len,
                                },
                            }));
                        }
                        b"symlink" => {
                            self.expect_token("target").await?;
                            let target = self.read_string().await?;
                            self.state = State::CloseNode {
                                closes: u32::from(wrapper) + 1,
                            };
                            return Ok(Some(Entry {
                                path: self.current_path()?,
                                kind: EntryKind::Symlink {
                                    target: String::from_utf8(target).map_err(|_| {
                                        NarError::InvalidName("symlink target is not UTF-8".into())
                                    })?,
                                },
                            }));
                        }
                        b"directory" => {
                            let path = self.current_path()?;
                            let name = String::from_utf8(std::mem::take(&mut self.pending))
                                .map_err(|_| {
                                    NarError::InvalidName("entry name is not UTF-8".into())
                                })?;
                            self.frames.push(DirFrame {
                                name,
                                last_name: Vec::new(),
                            });
                            self.state = State::InDirectory;
                            return Ok(Some(Entry {
                                path,
                                kind: EntryKind::Directory,
                            }));
                        }
                        other => {
                            return Err(NarError::UnexpectedToken {
                                expected: "regular|symlink|directory",
                                got: String::from_utf8_lossy(other).into_owned(),
                            });
                        }
                    }
                }
                State::InDirectory => {
                    let tok = self.read_string().await?;
                    match tok.as_slice() {
                        b")" => {
                            // closes the directory node itself; root has no
                            // entry wrapper, non-root nodes have one
                            self.frames.pop().expect("directory frame");
                            if self.frames.is_empty() {
                                self.state = State::Finished;
                                return Ok(None);
                            }
                            self.expect_token(")").await?;
                            self.state = State::InDirectory;
                        }
                        b"entry" => {
                            self.expect_token("(").await?;
                            self.expect_token("name").await?;
                            let name = self.read_string().await?;
                            self.expect_token("node").await?;
                            let frame = self.frames.last_mut().expect("directory frame");
                            if name.is_empty()
                                || name == b"."
                                || name == b".."
                                || name.contains(&b'/')
                                || name.contains(&0)
                            {
                                return Err(NarError::InvalidName(
                                    String::from_utf8_lossy(&name).into_owned(),
                                ));
                            }
                            if name <= frame.last_name {
                                return Err(NarError::UnsortedEntries(
                                    String::from_utf8_lossy(&name).into_owned(),
                                ));
                            }
                            frame.last_name = name.clone();
                            self.pending = name;
                            self.state = State::ExpectNode { wrapper: true };
                        }
                        other => {
                            return Err(NarError::UnexpectedToken {
                                expected: "entry|)",
                                got: String::from_utf8_lossy(other).into_owned(),
                            });
                        }
                    }
                }
                State::CloseNode { closes } => {
                    // skip any unread contents + padding, then close parens
                    self.scratch_capacity();
                    let mut left = self.data_remaining + pad(self.data_total);
                    while left > 0 {
                        let n = left.min(self.scratch.len() as u64) as usize;
                        let mut chunk = std::mem::take(&mut self.scratch);
                        let res = self.read_exact(&mut chunk[..n]).await;
                        let read_ok = res.is_ok();
                        self.scratch = chunk;
                        if !read_ok {
                            return res.map(|_| None);
                        }
                        left -= n as u64;
                    }
                    self.data_remaining = 0;
                    self.data_total = 0;
                    for _ in 0..closes {
                        self.expect_token(")").await?;
                    }
                    self.pending.clear();
                    if self.frames.is_empty() {
                        self.state = State::Finished;
                        return Ok(None);
                    }
                    self.state = State::InDirectory;
                }
                State::Finished => return Ok(None),
            }
        }
    }

    /// Reader for the current regular file's contents. Valid after a
    /// `Regular` entry and before the next call to `next_entry`.
    pub fn data(&mut self) -> NarData<'_, R> {
        NarData {
            src: &mut self.src,
            remaining: &mut self.data_remaining,
        }
    }

    fn current_path(&self) -> Result<String> {
        let mut path = String::new();
        for frame in &self.frames {
            if !frame.name.is_empty() {
                path.push_str(&frame.name);
                path.push('/');
            }
        }
        if !self.pending.is_empty() {
            path.push_str(
                &String::from_utf8(self.pending.clone())
                    .map_err(|_| NarError::InvalidName("entry name is not UTF-8".into()))?,
            );
        }
        Ok(path)
    }

    fn scratch_capacity(&mut self) -> usize {
        if self.scratch.len() < 64 << 10 {
            self.scratch.resize(64 << 10, 0);
        }
        64 << 10
    }

    async fn read_u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_exact(&mut b).await?;
        Ok(u64::from_le_bytes(b))
    }

    /// Reads a length-prefixed padded string.
    async fn read_string(&mut self) -> Result<Vec<u8>> {
        let len = self.read_u64().await?;
        if len > MAX_TOKEN {
            return Err(NarError::TokenTooLarge {
                limit: MAX_TOKEN as usize,
            });
        }
        let mut buf = vec![0u8; len as usize];
        self.read_exact(&mut buf).await?;
        let padding = pad(len) as usize;
        if padding > 0 {
            let mut p = vec![0u8; padding];
            self.read_exact(&mut p).await?;
        }
        Ok(buf)
    }

    async fn expect_token(&mut self, expected: &'static str) -> Result<()> {
        let got = self.read_string().await?;
        if got != expected.as_bytes() {
            return Err(NarError::UnexpectedToken {
                expected,
                got: String::from_utf8_lossy(&got).into_owned(),
            });
        }
        Ok(())
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        match self.src.read_exact(buf).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(NarError::Eof),
            Err(e) => Err(e.into()),
        }
    }
}

/// Reader over the current file's contents; does not consume the padding.
pub struct NarData<'a, R> {
    src: &'a mut R,
    remaining: &'a mut u64,
}

impl<R: AsyncRead + Unpin> AsyncRead for NarData<'_, R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if *this.remaining == 0 || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let want = (*this.remaining).min(buf.remaining() as u64) as usize;
        let mut slice = tokio::io::ReadBuf::new(buf.initialize_unfilled_to(want));
        match Pin::new(&mut *this.src).poll_read(cx, &mut slice) {
            Poll::Ready(Ok(())) => {
                let n = slice.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "nar contents truncated",
                    )));
                }
                *this.remaining -= n as u64;
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn meta_dir() -> InodeMeta {
    InodeMeta::dir(0o555)
}

fn meta_reg(executable: bool) -> InodeMeta {
    InodeMeta::reg(if executable { 0o555 } else { 0o444 })
}

fn meta_symlink() -> InodeMeta {
    InodeMeta::symlink()
}

fn join(prefix: Option<&str>, path: &str) -> Result<String> {
    match prefix {
        None => Ok(path.to_string()),
        Some(p) if p.is_empty() => Ok(path.to_string()),
        Some(p) => Ok(format!("{p}/{path}")),
    }
}

/// Writes a NAR stream into an EROFS image, optionally placing all entries
/// under `prefix` (e.g. `store/abc-name` when the image is mounted at `/nix`).
/// A regular file at the NAR root can only be written when `prefix` is set;
/// it then lands at `prefix` itself.
pub async fn write_nar<R, W>(
    nar: &mut NarDecoder<R>,
    writer: &mut Writer<W>,
    prefix: Option<&str>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + AsyncSeek + Unpin + Send,
{
    while let Some(entry) = nar.next_entry().await? {
        match &entry.kind {
            EntryKind::Directory => {
                let path = join(prefix, &entry.path)?;
                if path.is_empty() {
                    // the NAR root directory is the image root itself
                    continue;
                }
                writer.mkdir(&path, meta_dir()).await?;
            }
            EntryKind::Regular { executable, size } => {
                let path = join(prefix, &entry.path)?;
                if path.is_empty() {
                    return Err(NarError::RootFileNeedsPrefix);
                }
                writer
                    .add_file(&path, meta_reg(*executable), *size, &mut nar.data())
                    .await?;
            }
            EntryKind::Symlink { target } => {
                let path = join(prefix, &entry.path)?;
                if path.is_empty() {
                    return Err(NarError::RootFileNeedsPrefix);
                }
                writer
                    .symlink(&path, target.as_bytes(), meta_symlink())
                    .await?;
            }
        }
    }
    Ok(())
}

/// Convenience: decode `src` into a fresh EROFS image on `sink`, using
/// reproducible (epoch) build times. Returns the finished sink.
pub async fn nar_to_image<R, W>(src: R, sink: W, prefix: Option<&str>) -> Result<W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + AsyncSeek + Unpin + Send,
{
    let mut writer = image_writer(sink, "").await?;
    let mut nar = NarDecoder::new(src);
    write_nar(&mut nar, &mut writer, prefix).await?;
    let sink = writer.finish().await?;
    Ok(sink.into_inner())
}

/// Reproducible (epoch) `CreateOptions` for the given volume name — the
/// conventions every image produced by flaky follows.
pub fn image_writer_opts(volume: &str) -> CreateOptions {
    CreateOptions {
        block_size: DEFAULT_BLOCK_SIZE,
        build_time: 0,
        build_time_nsec: 0,
        volume_name: volume.to_string(),
        ..Default::default()
    }
}

/// A `Writer` over `sink` counting bytes written, per `image_writer_opts`.
pub async fn image_writer<W>(sink: W, volume: &str) -> Result<Writer<CountingSink<W>>>
where
    W: AsyncWrite + AsyncSeek + Unpin + Send,
{
    Ok(Writer::new(CountingSink::new(sink), image_writer_opts(volume)).await?)
}

/// Finishes `writer`, returning the unwrapped sink and the image size in
/// bytes (the high-water mark, not the patched-back stream position).
pub async fn finish_image<W>(writer: Writer<CountingSink<W>>) -> Result<(W, u64)>
where
    W: AsyncWrite + AsyncSeek + Unpin + Send,
{
    let sink = writer.finish().await?;
    let size = sink.count();
    Ok((sink.into_inner(), size))
}

/// Sink wrapper that tracks the high-water mark of the underlying write
/// position. `Writer::finish` leaves the stream position at the patched
/// superblock (offset 0), so the real image size is the highest byte offset
/// ever written; `count()` returns it.
pub struct CountingSink<W> {
    inner: W,
    pos: u64,
    max: u64,
}

impl<W> CountingSink<W> {
    pub fn new(inner: W) -> Self {
        CountingSink {
            inner,
            pos: 0,
            max: 0,
        }
    }

    /// Highest write end-offset seen (the image length).
    pub fn count(&self) -> u64 {
        self.max
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CountingSink<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                this.pos += n as u64;
                this.max = this.max.max(this.pos);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl<W: AsyncSeek + Unpin> AsyncSeek for CountingSink<W> {
    fn start_seek(self: Pin<&mut Self>, pos: SeekFrom) -> std::io::Result<()> {
        let this = self.get_mut();
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => this.pos as i64 + d,
            SeekFrom::End(_) => {
                return Err(std::io::Error::other(
                    "CountingSink: seek from end unsupported",
                ));
            }
        };
        this.pos = target.max(0) as u64;
        Pin::new(&mut this.inner).start_seek(pos)
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut self.get_mut().inner).poll_complete(cx)
    }
}

#[cfg(test)]
mod tests;
