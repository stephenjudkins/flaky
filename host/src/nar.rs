//! Serializing a real directory tree as a NAR stream (an `AsyncRead`),
//! plus a sha256-hashing reader wrapper. Used to pack fetched flake inputs
//! into erofs images while verifying their lock narHash.

use std::fs;
use std::io;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, ReadBuf};

fn token(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s);
    out.extend(std::iter::repeat_n(0, (8 - (s.len() % 8)) % 8));
}

fn token_str(out: &mut Vec<u8>, s: &str) {
    token(out, s.as_bytes());
}

enum Frame {
    Dir {
        entries: std::vec::IntoIter<(Vec<u8>, PathBuf)>,
        wrapped: bool,
    },
}

struct FileState {
    file: fs::File,
    remaining: u64,
    pad: u64,
}

pub struct DirNar {
    root: Option<PathBuf>,
    stack: Vec<Frame>,
    file: Option<FileState>,
    pending: Vec<u8>,
    pos: usize,
    tmp: Vec<u8>,
    started: bool,
    done: bool,
}

impl DirNar {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirNar {
            root: Some(root.into()),
            stack: Vec::new(),
            file: None,
            pending: Vec::new(),
            pos: 0,
            tmp: vec![0u8; 64 * 1024],
            started: false,
            done: false,
        }
    }

    fn descend(&mut self, path: &Path, wrapped: bool) -> io::Result<()> {
        let md = fs::symlink_metadata(path)?;
        let ft = md.file_type();
        let out = &mut self.pending;
        if ft.is_dir() {
            token_str(out, "(");
            token_str(out, "type");
            token_str(out, "directory");
            let mut entries: Vec<(Vec<u8>, PathBuf)> = fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .map(|e| (e.file_name().as_encoded_bytes().to_vec(), e.path()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            self.stack.push(Frame::Dir {
                entries: entries.into_iter(),
                wrapped,
            });
        } else if ft.is_file() {
            token_str(out, "(");
            token_str(out, "type");
            token_str(out, "regular");
            if md.mode() & 0o111 != 0 {
                token_str(out, "executable");
                token(out, &[]);
            }
            token_str(out, "contents");
            out.extend_from_slice(&md.len().to_le_bytes());
            self.file = Some(FileState {
                file: fs::File::open(path)?,
                remaining: md.len(),
                pad: (8 - (md.len() % 8)) % 8,
            });
        } else if ft.is_symlink() {
            token_str(out, "(");
            token_str(out, "type");
            token_str(out, "symlink");
            token_str(out, "target");
            let target = fs::read_link(path)?;
            token(out, target.as_os_str().as_encoded_bytes());
            token_str(out, ")");
            if wrapped {
                token_str(out, ")");
            }
        } else {
            return Err(io::Error::other(format!(
                "unsupported file kind in nar tree: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn advance(&mut self) -> io::Result<()> {
        const CAP: usize = 256 * 1024;
        if self.pos > 0 {
            self.pending.drain(..self.pos);
            self.pos = 0;
        }
        while !self.done && self.pending.len() < CAP {
            if self.file.is_some() {
                let mut f = self.file.take().unwrap();
                let want = (CAP - self.pending.len())
                    .min(f.remaining as usize)
                    .min(self.tmp.len());
                if want > 0 {
                    let mut read = 0usize;
                    while read < want {
                        match f.file.read(&mut self.tmp[..want - read]) {
                            Ok(0) => {
                                return Err(io::Error::other("file truncated while packing nar"));
                            }
                            Ok(n) => {
                                self.pending.extend_from_slice(&self.tmp[..n]);
                                read += n;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    f.remaining -= want as u64;
                }
                if f.remaining == 0 {
                    let pad = f.pad as usize;
                    self.pending.extend(std::iter::repeat_n(0, pad));
                    token(&mut self.pending, b")");
                    token(&mut self.pending, b")");
                } else {
                    self.file = Some(f);
                }
                continue;
            }
            if !self.started {
                self.started = true;
                token_str(&mut self.pending, "nix-archive-1");
                let root = self.root.take().expect("root");
                self.descend(&root, false)?;
                continue;
            }
            let next = match self.stack.last_mut() {
                Some(Frame::Dir { entries, .. }) => entries.next(),
                None => unreachable!("stack empty before done"),
            };
            if let Some((name, path)) = next {
                token_str(&mut self.pending, "entry");
                token_str(&mut self.pending, "(");
                token_str(&mut self.pending, "name");
                token(&mut self.pending, &name);
                token_str(&mut self.pending, "node");
                self.descend(&path, true)?;
            } else {
                token_str(&mut self.pending, ")");
                let wrapped = match self.stack.last() {
                    Some(Frame::Dir { wrapped, .. }) => *wrapped,
                    None => unreachable!(),
                };
                if wrapped {
                    token_str(&mut self.pending, ")");
                }
                self.stack.pop();
                if self.stack.is_empty() {
                    self.done = true;
                }
            }
        }
        Ok(())
    }
}

impl AsyncRead for DirNar {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos == self.pending.len() {
            if self.done {
                return Poll::Ready(Ok(()));
            }
            match self.advance() {
                Ok(()) => {
                    if self.pos == self.pending.len() && self.done {
                        return Poll::Ready(Ok(()));
                    }
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        let n = buf.remaining().min(self.pending.len() - self.pos);
        let pos = self.pos;
        buf.put_slice(&self.pending[pos..pos + n]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

pub struct HashReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R> HashReader<R> {
    pub fn new(inner: R) -> Self {
        HashReader {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn digest(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for HashReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let filled = buf.filled().len();
                if filled > 0 {
                    this.hasher.update(buf.filled());
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests;
