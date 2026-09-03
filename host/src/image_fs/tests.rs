use std::io::Write;

use alioth::fuse::bindings::FuseOpcode;
use tempfile::TempDir;

use super::*;

fn header(nodeid: u64, opcode: FuseOpcode) -> FuseInHeader {
    FuseInHeader {
        len: 0,
        opcode,
        unique: 1,
        nodeid,
        uid: 0,
        gid: 0,
        pid: 0,
        total_extlen: 0,
        padding: 0,
    }
}

#[test]
fn exposes_image_by_name_and_reads_at_offset() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("image.erofs");
    File::create(&path)
        .unwrap()
        .write_all(b"0123456789")
        .unwrap();
    let mut fs = ImageFiles::new(vec![ImageFile {
        name: "abc.erofs".to_string(),
        path,
    }])
    .unwrap();

    let entry = fs
        .lookup(&header(FUSE_ROOT_ID, FuseOpcode::LOOKUP), b"abc.erofs\0")
        .unwrap();
    assert_eq!(entry.attr.size, 10);
    assert_eq!(entry.attr.mode & libc::S_IFMT as u32, libc::S_IFREG as u32);

    let opened = fs
        .open(
            &header(entry.nodeid, FuseOpcode::OPEN),
            &FuseOpenIn {
                flags: libc::O_RDONLY as u32,
                open_flags: 0,
            },
        )
        .unwrap();
    let mut bytes = [0; 4];
    let mut output = [IoSliceMut::new(&mut bytes)];
    let read = fs
        .read(
            &header(entry.nodeid, FuseOpcode::READ),
            &FuseReadIn {
                fh: opened.fh,
                offset: 3,
                size: 4,
                ..Default::default()
            },
            &mut output,
        )
        .unwrap();
    assert_eq!(read, 4);
    assert_eq!(&bytes, b"3456");
}

#[test]
fn rejects_duplicate_and_unsafe_names() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("image.erofs");
    File::create(&path).unwrap();

    assert!(
        ImageFiles::new(vec![
            ImageFile {
                name: "same".to_string(),
                path: path.clone(),
            },
            ImageFile {
                name: "same".to_string(),
                path: path.clone(),
            },
        ])
        .is_err()
    );
    assert!(
        ImageFiles::new(vec![ImageFile {
            name: "../image".to_string(),
            path,
        }])
        .is_err()
    );
}
