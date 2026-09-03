use super::*;
use std::io::Cursor;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};

fn s(x: &str) -> Vec<u8> {
    let b = x.as_bytes();
    let mut out = (b.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(b);
    let p = (8 - (b.len() % 8)) % 8;
    out.extend(std::iter::repeat_n(0, p));
    out
}

fn u64le(n: u64) -> Vec<u8> {
    n.to_le_bytes().to_vec()
}

fn contents(data: &[u8]) -> Vec<u8> {
    let mut out = u64le(data.len() as u64);
    out.extend_from_slice(data);
    let p = (8 - (data.len() % 8)) % 8;
    out.extend(std::iter::repeat_n(0, p));
    out
}

/// Root directory with two files, a symlink, and a nested directory.
fn tree_nar() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(s("nix-archive-1"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("directory"));
    // entry: a.txt (13 bytes -> padded to 16)
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("a.txt"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("regular"));
    v.extend(s("contents"));
    v.extend(contents(b"Hello, World!"));
    v.extend(s(")"));
    v.extend(s(")"));
    // entry: b.sh executable empty
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("b.sh"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("regular"));
    v.extend(s("executable"));
    v.extend(s(""));
    v.extend(s("contents"));
    v.extend(contents(b""));
    v.extend(s(")"));
    v.extend(s(")"));
    // entry: link -> a.txt
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("link"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("symlink"));
    v.extend(s("target"));
    v.extend(s("a.txt"));
    v.extend(s(")"));
    v.extend(s(")"));
    // entry: sub/ directory containing c.txt
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("sub"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("directory"));
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("c.txt"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("regular"));
    v.extend(s("contents"));
    v.extend(contents(b"nested"));
    v.extend(s(")"));
    v.extend(s(")"));
    // entry: sub/deeper/ directory containing f.txt (depth-3 regression)
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("deeper"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("directory"));
    v.extend(s("entry"));
    v.extend(s("("));
    v.extend(s("name"));
    v.extend(s("f.txt"));
    v.extend(s("node"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("regular"));
    v.extend(s("contents"));
    v.extend(contents(b"deep"));
    v.extend(s(")"));
    v.extend(s(")"));
    v.extend(s(")"));
    v.extend(s(")"));
    v.extend(s(")"));
    v.extend(s(")"));
    v.extend(s(")"));
    v
}

async fn entries(nar: Vec<u8>) -> Vec<Entry> {
    let mut dec = NarDecoder::new(Cursor::new(nar));
    let mut out = Vec::new();
    while let Some(e) = dec.next_entry().await.unwrap() {
        out.push(e);
    }
    out
}

#[tokio::test]
async fn decodes_tree() {
    let es = entries(tree_nar()).await;
    assert_eq!(
        es,
        vec![
            Entry {
                path: "".into(),
                kind: EntryKind::Directory
            },
            Entry {
                path: "a.txt".into(),
                kind: EntryKind::Regular {
                    executable: false,
                    size: 13
                }
            },
            Entry {
                path: "b.sh".into(),
                kind: EntryKind::Regular {
                    executable: true,
                    size: 0
                }
            },
            Entry {
                path: "link".into(),
                kind: EntryKind::Symlink {
                    target: "a.txt".into()
                }
            },
            Entry {
                path: "sub".into(),
                kind: EntryKind::Directory
            },
            Entry {
                path: "sub/c.txt".into(),
                kind: EntryKind::Regular {
                    executable: false,
                    size: 6
                }
            },
            Entry {
                path: "sub/deeper".into(),
                kind: EntryKind::Directory
            },
            Entry {
                path: "sub/deeper/f.txt".into(),
                kind: EntryKind::Regular {
                    executable: false,
                    size: 4
                }
            },
        ]
    );
}

#[tokio::test]
async fn reads_contents() {
    let mut dec = NarDecoder::new(Cursor::new(tree_nar()));
    while let Some(e) = dec.next_entry().await.unwrap() {
        if e.path == "a.txt" {
            let mut buf = Vec::new();
            dec.data().read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf, b"Hello, World!");
        }
        if e.path == "sub/c.txt" {
            let mut buf = Vec::new();
            dec.data().read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf, b"nested");
        }
    }
}

#[tokio::test]
async fn partial_reads_align() {
    // read only part of the contents; the decoder must skip the rest
    let mut dec = NarDecoder::new(Cursor::new(tree_nar()));
    let mut seen = Vec::new();
    while let Some(e) = dec.next_entry().await.unwrap() {
        if let EntryKind::Regular { size, .. } = e.kind {
            let mut buf = vec![0u8; (size / 2) as usize];
            if !buf.is_empty() {
                dec.data().read_exact(&mut buf).await.unwrap();
            }
            seen.push(e.path);
        }
    }
    assert_eq!(seen, ["a.txt", "b.sh", "sub/c.txt", "sub/deeper/f.txt"]);
}

#[tokio::test]
async fn root_regular_file() {
    let mut v = Vec::new();
    v.extend(s("nix-archive-1"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("regular"));
    v.extend(s("contents"));
    v.extend(contents(b"file at root"));
    v.extend(s(")"));
    let es = entries(v.clone()).await;
    assert_eq!(
        es,
        vec![Entry {
            path: "".into(),
            kind: EntryKind::Regular {
                executable: false,
                size: 12
            }
        }]
    );
    // writing without a prefix must fail, with a prefix it must work
    let sink = std::io::Cursor::new(Vec::new());
    assert!(matches!(
        nar_to_image(Cursor::new(v.clone()), sink, None).await,
        Err(NarError::RootFileNeedsPrefix)
    ));
    let sink = std::io::Cursor::new(Vec::new());
    assert!(
        nar_to_image(Cursor::new(v), sink, Some("file-store-path"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn rejects_bad_magic() {
    let mut v = Vec::new();
    v.extend(s("not-a-nar"));
    v.extend(s("("));
    let mut dec = NarDecoder::new(Cursor::new(v));
    assert!(matches!(dec.next_entry().await, Err(NarError::BadMagic)));
}

#[tokio::test]
async fn rejects_unsorted() {
    let mut v = Vec::new();
    v.extend(s("nix-archive-1"));
    v.extend(s("("));
    v.extend(s("type"));
    v.extend(s("directory"));
    for name in ["b", "a"] {
        v.extend(s("entry"));
        v.extend(s("("));
        v.extend(s("name"));
        v.extend(s(name));
        v.extend(s("node"));
        v.extend(s("("));
        v.extend(s("type"));
        v.extend(s("regular"));
        v.extend(s("contents"));
        v.extend(contents(b"x"));
        v.extend(s(")"));
        v.extend(s(")"));
    }
    v.extend(s(")"));
    v.extend(s(")"));
    let mut dec = NarDecoder::new(Cursor::new(v));
    dec.next_entry().await.unwrap(); // root dir
    dec.next_entry().await.unwrap(); // b
    assert!(matches!(
        dec.next_entry().await,
        Err(NarError::UnsortedEntries(_))
    ));
}

#[tokio::test]
async fn rejects_truncated() {
    let full = tree_nar();
    let mut dec = NarDecoder::new(Cursor::new(&full[..full.len() - 9]));
    loop {
        match dec.next_entry().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("archive ended cleanly despite truncation"),
            Err(NarError::Eof) => break,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

// ---- image writing ----

const EROFS_MAGIC_OFFSET: usize = 1024;

#[tokio::test]
async fn writes_erofs_image() {
    let sink = std::io::Cursor::new(Vec::new());
    let sink = nar_to_image(Cursor::new(tree_nar()), sink, None)
        .await
        .unwrap();
    let img = sink.into_inner();
    assert!(img.len() > EROFS_MAGIC_OFFSET + 4);
    let magic = u32::from_le_bytes([
        img[EROFS_MAGIC_OFFSET],
        img[EROFS_MAGIC_OFFSET + 1],
        img[EROFS_MAGIC_OFFSET + 2],
        img[EROFS_MAGIC_OFFSET + 3],
    ]);
    assert_eq!(magic, 0xE0F5E1E2, "erofs superblock magic");
    // volume_name sits at sb offset 64, 16 bytes
    let vn = &img[EROFS_MAGIC_OFFSET + 64..EROFS_MAGIC_OFFSET + 80];
    assert!(vn.iter().all(|&b| b == 0), "no volume name by default");
}

#[tokio::test]
async fn writes_erofs_image_with_prefix() {
    let sink = std::io::Cursor::new(Vec::new());
    let sink = nar_to_image(Cursor::new(tree_nar()), sink, Some("store/abc-hello"))
        .await
        .unwrap();
    assert!(sink.into_inner().len() > EROFS_MAGIC_OFFSET + 84);
}

#[tokio::test]
async fn counting_sink_tracks_high_water_mark() {
    let sink = CountingSink::new(std::io::Cursor::new(Vec::new()));
    let mut sink = sink;
    sink.write_all(b"hello").await.unwrap(); // pos 5, max 5
    sink.seek(SeekFrom::Start(100)).await.unwrap();
    sink.write_all(b"xy").await.unwrap(); // pos 102, max 102
    sink.seek(SeekFrom::Start(0)).await.unwrap();
    sink.write_all(&[0u8; 8]).await.unwrap(); // pos 8, max stays 102
    assert_eq!(sink.count(), 102);
    let inner = sink.into_inner();
    assert_eq!(inner.get_ref().len(), 102);
}
