use std::collections::{BTreeMap, VecDeque};
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, IoSliceMut};

use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use alioth::fuse::bindings::{
    FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_ROOT_ID, FuseAttr, FuseAttrOut,
    FuseEntryOut, FuseForgetIn, FuseGetattrIn, FuseInHeader, FuseInitIn, FuseInitOut, FuseOpenIn,
    FuseOpenOut, FuseReadIn, FuseReleaseIn,
};
use alioth::fuse::{DaxRegion, Fuse};
use alioth::virtio::dev::DevParam;
use alioth::virtio::dev::fs::{Fs, FsConfig};

const MAX_BUFFER_SIZE: u32 = 1 << 20;
const FILE_CACHE_SIZE: usize = 128;

#[derive(Debug)]
pub struct ImageFile {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug)]
struct Image {
    path: PathBuf,
    size: u64,
    dev: u64,
    ino: u64,
}

#[derive(Debug)]
pub struct ImageFiles {
    names: BTreeMap<Vec<u8>, usize>,
    images: Vec<Image>,
    files: VecDeque<(usize, File)>,
}

impl ImageFiles {
    pub fn new(files: Vec<ImageFile>) -> io::Result<Self> {
        let mut names = BTreeMap::new();
        let mut images = Vec::with_capacity(files.len());
        for file in files {
            if file.name.is_empty()
                || file.name.as_bytes().contains(&0)
                || file.name.as_bytes().contains(&b'/')
                || file.name == "."
                || file.name == ".."
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid image filename {:?}", file.name),
                ));
            }
            let opened = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&file.path)?;
            let meta = opened.metadata()?;
            if !meta.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("image is not a regular file: {}", file.path.display()),
                ));
            }
            let index = images.len();
            if names.insert(file.name.into_bytes(), index).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate image filename",
                ));
            }
            images.push(Image {
                path: file.path,
                size: meta.len(),
                dev: meta.dev(),
                ino: meta.ino(),
            });
        }
        Ok(Self {
            names,
            images,
            files: VecDeque::new(),
        })
    }

    fn image_index(nodeid: u64) -> Option<usize> {
        nodeid.checked_sub(FUSE_ROOT_ID + 1)?.try_into().ok()
    }

    fn image(&self, nodeid: u64) -> alioth::fuse::Result<&Image> {
        let Some(index) = Self::image_index(nodeid) else {
            return Err(io::Error::from_raw_os_error(libc::ENOENT))?;
        };
        self.images
            .get(index)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT).into())
    }

    fn attr(&self, nodeid: u64) -> alioth::fuse::Result<FuseAttr> {
        if nodeid == FUSE_ROOT_ID {
            return Ok(FuseAttr {
                ino: FUSE_ROOT_ID,
                mode: libc::S_IFDIR as u32 | 0o500,
                nlink: 2,
                blksize: 4096,
                ..Default::default()
            });
        }
        let image = self.image(nodeid)?;
        Ok(FuseAttr {
            ino: nodeid,
            size: image.size,
            blocks: image.size.div_ceil(512),
            mode: libc::S_IFREG as u32 | 0o400,
            nlink: 1,
            blksize: 4096,
            ..Default::default()
        })
    }

    fn open_file(&mut self, index: usize) -> alioth::fuse::Result<&File> {
        if let Some(position) = self.files.iter().position(|(i, _)| *i == index) {
            let entry = self.files.remove(position).unwrap();
            self.files.push_back(entry);
            return Ok(&self.files.back().unwrap().1);
        }

        let image = self
            .images
            .get(index)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&image.path)?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.len() != image.size
            || meta.dev() != image.dev
            || meta.ino() != image.ino
        {
            return Err(io::Error::other(format!(
                "image changed while VM was running: {}",
                image.path.display()
            )))?;
        }
        if self.files.len() == FILE_CACHE_SIZE {
            self.files.pop_front();
        }
        self.files.push_back((index, file));
        Ok(&self.files.back().unwrap().1)
    }
}

impl Fuse for ImageFiles {
    fn init(
        &mut self,
        _hdr: &FuseInHeader,
        input: &FuseInitIn,
    ) -> alioth::fuse::Result<FuseInitOut> {
        Ok(FuseInitOut {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: input.max_readahead,
            flags: 0,
            max_background: u16::MAX,
            congestion_threshold: (u16::MAX / 4) * 3,
            max_write: MAX_BUFFER_SIZE,
            time_gran: 1,
            max_pages: 256,
            ..Default::default()
        })
    }

    fn get_attr(
        &mut self,
        hdr: &FuseInHeader,
        _input: &FuseGetattrIn,
    ) -> alioth::fuse::Result<FuseAttrOut> {
        Ok(FuseAttrOut {
            attr_valid: 3600,
            attr: self.attr(hdr.nodeid)?,
            ..Default::default()
        })
    }

    fn lookup(&mut self, hdr: &FuseInHeader, input: &[u8]) -> alioth::fuse::Result<FuseEntryOut> {
        if hdr.nodeid != FUSE_ROOT_ID {
            return Err(io::Error::from_raw_os_error(libc::ENOENT))?;
        }
        let name = CStr::from_bytes_until_nul(input)?;
        let Some(index) = self.names.get(name.to_bytes()).copied() else {
            return Err(io::Error::from_raw_os_error(libc::ENOENT))?;
        };
        let nodeid = index as u64 + FUSE_ROOT_ID + 1;
        Ok(FuseEntryOut {
            nodeid,
            entry_valid: 3600,
            attr_valid: 3600,
            attr: self.attr(nodeid)?,
            ..Default::default()
        })
    }

    fn forget(&mut self, _hdr: &FuseInHeader, _input: &FuseForgetIn) -> alioth::fuse::Result<()> {
        Ok(())
    }

    fn open(
        &mut self,
        hdr: &FuseInHeader,
        input: &FuseOpenIn,
    ) -> alioth::fuse::Result<FuseOpenOut> {
        self.image(hdr.nodeid)?;
        if input.flags as i32 & libc::O_ACCMODE != libc::O_RDONLY
            || input.flags as i32 & (libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND) != 0
        {
            return Err(io::Error::from_raw_os_error(libc::EROFS))?;
        }
        Ok(FuseOpenOut {
            fh: hdr.nodeid,
            ..Default::default()
        })
    }

    fn read(
        &mut self,
        hdr: &FuseInHeader,
        input: &FuseReadIn,
        output: &mut [IoSliceMut],
    ) -> alioth::fuse::Result<usize> {
        if input.fh != hdr.nodeid {
            return Err(io::Error::from_raw_os_error(libc::EBADF))?;
        }
        let index = Self::image_index(hdr.nodeid)
            .filter(|i| *i < self.images.len())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let file = self.open_file(index)?;
        let mut total = 0usize;
        let limit = input.size as usize;
        for buf in output {
            if total == limit {
                break;
            }
            let len = buf.len().min(limit - total);
            let read = file.read_at(&mut buf[..len], input.offset + total as u64)?;
            total += read;
            if read < len {
                break;
            }
        }
        Ok(total)
    }

    fn release(&mut self, hdr: &FuseInHeader, input: &FuseReleaseIn) -> alioth::fuse::Result<()> {
        if input.fh != hdr.nodeid {
            return Err(io::Error::from_raw_os_error(libc::EBADF))?;
        }
        Ok(())
    }

    fn set_dax_region(&mut self, _dax_region: Box<dyn DaxRegion>) {}
}

#[derive(Debug)]
pub struct ImageFilesParam {
    pub tag: String,
    pub files: Vec<ImageFile>,
}

impl DevParam for ImageFilesParam {
    type Device = Fs<ImageFiles>;

    fn build(self, name: impl Into<Arc<str>>) -> Result<Self::Device, alioth::virtio::Error> {
        let filesystem = ImageFiles::new(self.files)?;
        let mut config = FsConfig {
            tag: [0; 36],
            num_request_queues: 1,
            notify_buf_size: 0,
        };
        if self.tag.len() > config.tag.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtio-fs tag is too long",
            ))?;
        }
        config.tag[..self.tag.len()].copy_from_slice(self.tag.as_bytes());
        Fs::new(name, filesystem, config, 0)
    }
}


#[cfg(test)]
mod tests;
