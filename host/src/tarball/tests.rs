use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::nar::DirNar;

enum Node {
    Dir(&'static [(&'static str, Node)]),
    File(&'static [u8]),
    Exec(&'static [u8]),
    Symlink(&'static str),
}

// git tree order: directories sort as `name/`, so `x.md` precedes `x/`
static TREE: &[(&str, Node)] = &[
    ("a.txt", Node::File(b"alpha")),
    ("empty.bin", Node::File(b"")),
    ("link", Node::Symlink("a.txt")),
    (
        "sub",
        Node::Dir(&[
            ("deep.txt", Node::File(b"deep")),
            ("run.sh", Node::Exec(b"#!/bin/sh\n")),
        ]),
    ),
    ("x.md", Node::File(b"markdown")),
    (
        "x",
        Node::Dir(&[
            ("child-exec", Node::Exec(b"exec")),
            ("empty-dir", Node::Dir(&[])),
            ("inner.txt", Node::File(b"inner")),
        ]),
    ),
    ("z-last", Node::File(b"omega")),
];

fn fill_header(header: &mut tar::Header, path: &str, node: &Node) {
    match node {
        Node::Dir(_) => {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
        }
        Node::File(data) => {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
        }
        Node::Exec(data) => {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o755);
            header.set_size(data.len() as u64);
        }
        Node::Symlink(target) => {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            header.set_link_name(target).unwrap();
        }
    }
    header.set_path(path).unwrap();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
}

fn entry_data(node: &Node) -> &[u8] {
    match node {
        Node::File(d) | Node::Exec(d) => d,
        _ => &[],
    }
}

fn git_order<'a>(prefix: &str, nodes: &'a [(&str, Node)], out: &mut Vec<(String, &'a Node)>) {
    let mut sorted: Vec<_> = nodes.iter().collect();
    sorted.sort_by_key(|(name, node)| match node {
        Node::Dir(_) => format!("{name}/"),
        _ => (*name).to_string(),
    });
    for (name, node) in sorted {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        out.push((path.clone(), node));
        if let Node::Dir(children) = node {
            git_order(&path, children, out);
        }
    }
}

fn build_tar(top: &str, flat: &[(String, &Node)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut append = |path: &str, node: &Node| {
        let mut header = tar::Header::new_gnu();
        fill_header(&mut header, path, node);
        builder
            .append_data(&mut header, path, entry_data(node))
            .unwrap();
    };
    append(&format!("{top}/"), &Node::Dir(&[]));
    for (path, node) in flat {
        append(path, node);
    }
    builder.into_inner().unwrap()
}

fn nar_byte_order<'a>(prefix: &str, nodes: &'a [(&str, Node)], out: &mut Vec<(String, &'a Node)>) {
    let mut sorted: Vec<_> = nodes.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, node) in sorted {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        out.push((path.clone(), node));
        if let Node::Dir(children) = node {
            nar_byte_order(&path, children, out);
        }
    }
}

fn materialize(root: &Path, nodes: &[(&str, Node)]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    for (name, node) in nodes {
        let path = root.join(name);
        match node {
            Node::Dir(children) => {
                std::fs::create_dir(&path)?;
                materialize(&path, children)?;
            }
            Node::File(data) => std::fs::write(&path, data)?,
            Node::Exec(data) => {
                std::fs::write(&path, data)?;
                let mut perms = std::fs::metadata(&path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(path, perms)?;
            }
            Node::Symlink(target) => std::os::unix::fs::symlink(target, &path)?,
        }
    }
    Ok(())
}

async fn dir_nar_digest(root: &Path) -> [u8; 32] {
    let mut nar = DirNar::new(root);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut nar, &mut buf)
            .await
            .unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.finalize().into()
}

async fn tree_digest() -> [u8; 32] {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    materialize(&tree, TREE).unwrap();
    dir_nar_digest(&tree).await
}

async fn run(tar_bytes: &[u8]) -> Result<[u8; 32], String> {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("img");
    let writer = super::tar_to_image(tar_bytes, "top-abc123", "store-base", "volume", &img).await?;
    let (file, size) = nar_to_erofs::finish_image(writer).await.unwrap();
    drop(file);
    assert!(size > 0);
    super::image_nar_digest(&img)
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn git_order_digest_matches_dir_nar_test() {
    let expected = tree_digest().await;
    let mut flat = Vec::new();
    git_order("top-abc123", TREE, &mut flat);
    let tar_bytes = build_tar("top-abc123", &flat);
    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn nar_order_digest_matches_dir_nar_test() {
    let expected = tree_digest().await;
    let mut flat = Vec::new();
    nar_byte_order("top-abc123", TREE, &mut flat);
    let tar_bytes = build_tar("top-abc123", &flat);
    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn truncated_stream_is_error_test() {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    header.set_path("top-abc123/").unwrap();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    builder
        .append_data(&mut header, "top-abc123/", std::io::empty())
        .unwrap();
    let data = vec![7u8; 100_000];
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    header.set_path("top-abc123/big").unwrap();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    builder
        .append_data(&mut header, "top-abc123/big", data.as_slice())
        .unwrap();
    let mut tar_bytes = builder.into_inner().unwrap();
    // cut inside the big entry's data, before the trailing zero blocks
    tar_bytes.truncate(tar_bytes.len() - 2000);
    assert!(run(&tar_bytes).await.is_err());
}

#[tokio::test]
async fn multi_file_park_test() {
    // two files (`x.md`, `x.txt`) both arrive before the `x/` directory and
    // must both be emitted after its subtree
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let x = tree.join("x");
    std::fs::create_dir(&x).unwrap();
    std::fs::write(x.join("inner"), b"i").unwrap();
    std::fs::write(tree.join("x.md"), b"m").unwrap();
    std::fs::write(tree.join("x.txt"), b"t").unwrap();
    let expected = dir_nar_digest(&tree).await;

    let mut builder = tar::Builder::new(Vec::new());
    let mut append = |path: &str, ty: tar::EntryType, mode: u32, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(ty);
        header.set_mode(mode);
        header.set_size(data.len() as u64);
        header.set_path(path).unwrap();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder.append_data(&mut header, path, data).unwrap();
    };
    append("top-abc123/", tar::EntryType::Directory, 0o755, &[]);
    append("top-abc123/x.md", tar::EntryType::Regular, 0o644, b"m");
    append("top-abc123/x.txt", tar::EntryType::Regular, 0o644, b"t");
    append("top-abc123/x/", tar::EntryType::Directory, 0o755, &[]);
    append("top-abc123/x/inner", tar::EntryType::Regular, 0o644, b"i");
    let tar_bytes = builder.into_inner().unwrap();

    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn hardlink_falls_back_test() {
    let mut builder = tar::Builder::new(Vec::new());
    let root: &[(&str, tar::EntryType, u32, &[u8], &str)] = &[
        ("top-abc123/", tar::EntryType::Directory, 0o755, &[], ""),
        ("top-abc123/a", tar::EntryType::Regular, 0o644, b"abc", ""),
        ("top-abc123/b", tar::EntryType::Link, 0o644, &[], "a"),
    ];
    for (path, ty, mode, data, link) in root {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*ty);
        header.set_mode(*mode);
        header.set_size(data.len() as u64);
        header.set_path(path).unwrap();
        if !link.is_empty() {
            header.set_link_name(link).unwrap();
        }
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder.append_data(&mut header, path, *data).unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();

    let err = run(&tar_bytes).await.unwrap_err();
    assert!(err.contains("unsupported"), "{err}");
}

#[tokio::test]
async fn arbitrary_order_test() {
    // arrival order must not matter: emission is driven by the index
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("z"), b"z-data").unwrap();
    std::fs::write(tree.join("a"), b"a-data").unwrap();
    let expected = dir_nar_digest(&tree).await;

    let mut builder = tar::Builder::new(Vec::new());
    let entries: &[(&str, tar::EntryType, u32, &[u8])] = &[
        ("top-abc123/", tar::EntryType::Directory, 0o755, &[]),
        ("top-abc123/z", tar::EntryType::Regular, 0o644, b"z-data"),
        ("top-abc123/a", tar::EntryType::Regular, 0o644, b"a-data"),
    ];
    for (path, ty, mode, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*ty);
        header.set_mode(*mode);
        header.set_size(data.len() as u64);
        header.set_path(path).unwrap();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder.append_data(&mut header, path, *data).unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();

    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn dir_dir_inversion_test() {
    // git order lists `x-b/` before `x/` ('-' < '/'); NAR needs `x`
    // first, and x-b's subtree arrived before x's header
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let xb = tree.join("x-b");
    std::fs::create_dir(&xb).unwrap();
    std::fs::write(xb.join("f"), b"b").unwrap();
    let x = tree.join("x");
    std::fs::create_dir(&x).unwrap();
    std::fs::write(x.join("inner"), b"i").unwrap();
    let expected = dir_nar_digest(&tree).await;

    let mut builder = tar::Builder::new(Vec::new());
    let entries: &[(&str, tar::EntryType, u32, &[u8])] = &[
        ("top-abc123/", tar::EntryType::Directory, 0o755, &[]),
        ("top-abc123/x-b/", tar::EntryType::Directory, 0o755, &[]),
        ("top-abc123/x-b/f", tar::EntryType::Regular, 0o644, b"b"),
        ("top-abc123/x/", tar::EntryType::Directory, 0o755, &[]),
        ("top-abc123/x/inner", tar::EntryType::Regular, 0o644, b"i"),
    ];
    for (path, ty, mode, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*ty);
        header.set_mode(*mode);
        header.set_size(data.len() as u64);
        header.set_path(path).unwrap();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder.append_data(&mut header, path, *data).unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();

    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn long_path_pax_test() {
    // paths over 100 bytes force pax extended headers, exercising the
    // offset accounting across entries tokio-tar consumes transparently
    let long_name = "l".repeat(140);
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join(&long_name), b"long").unwrap();
    std::fs::write(tree.join("short"), b"s").unwrap();
    let expected = dir_nar_digest(&tree).await;

    let mut builder = tar::Builder::new(Vec::new());
    let entries: Vec<(String, tar::EntryType, u32, Vec<u8>)> = vec![
        (
            "top-abc123/".into(),
            tar::EntryType::Directory,
            0o755,
            Vec::new(),
        ),
        (
            format!("top-abc123/{long_name}"),
            tar::EntryType::Regular,
            0o644,
            b"long".to_vec(),
        ),
        (
            "top-abc123/short".into(),
            tar::EntryType::Regular,
            0o644,
            b"s".to_vec(),
        ),
    ];
    for (path, ty, mode, data) in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*ty);
        header.set_mode(*mode);
        header.set_size(data.len() as u64);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder
            .append_data(&mut header, path, data.as_slice())
            .unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();
    // sanity: the long path really needed a pax extension
    assert!(tar_bytes.len() > 512 * 4);

    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn big_files_hashed_from_image_test() {
    // files over one block stream into the image and are hashed by
    // reading them back from it
    let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let big2: Vec<u8> = (0..5_000u32).map(|i| (i % 249) as u8).collect();
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("big"), &big).unwrap();
    std::fs::write(tree.join("big2"), &big2).unwrap();
    std::fs::write(tree.join("small"), b"s").unwrap();
    let expected = dir_nar_digest(&tree).await;

    let mut builder = tar::Builder::new(Vec::new());
    let mut append = |path: &str, ty: tar::EntryType, mode: u32, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(ty);
        header.set_mode(mode);
        header.set_size(data.len() as u64);
        header.set_path(path).unwrap();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder.append_data(&mut header, path, data).unwrap();
    };
    append("top-abc123/", tar::EntryType::Directory, 0o755, &[]);
    append("top-abc123/big", tar::EntryType::Regular, 0o644, &big);
    append("top-abc123/big2", tar::EntryType::Regular, 0o644, &big2);
    append("top-abc123/small", tar::EntryType::Regular, 0o644, b"s");
    let tar_bytes = builder.into_inner().unwrap();

    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn many_entries_test() {
    // enough entries to span multiple dirent blocks in the image
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    for i in 0..300 {
        let name = format!("entry-{i:04}-with-a-longish-name");
        std::fs::write(tree.join(&name), vec![(i % 7) as u8 * 17; (i % 7) * 100]).unwrap();
    }
    let expected = dir_nar_digest(&tree).await;

    let mut builder = tar::Builder::new(Vec::new());
    let mut append = |path: &str, ty: tar::EntryType, mode: u32, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(ty);
        header.set_mode(mode);
        header.set_size(data.len() as u64);
        header.set_path(path).unwrap();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder.append_data(&mut header, path, data).unwrap();
    };
    append("top-abc123/", tar::EntryType::Directory, 0o755, &[]);
    for i in 0..300 {
        let name = format!("entry-{i:04}-with-a-longish-name");
        let data = vec![(i % 7) as u8 * 17; (i % 7) * 100];
        append(
            &format!("top-abc123/{name}"),
            tar::EntryType::Regular,
            0o644,
            &data,
        );
    }
    let tar_bytes = builder.into_inner().unwrap();

    let got = run(&tar_bytes).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn image_matches_disk_path_byte_for_byte_test() {
    use crate::nar::{DirNar, HashReader};
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    materialize(&tree, TREE).unwrap();
    let mut flat = Vec::new();
    // arrival order drives the image layout now, so byte-identity with
    // the disk path holds only when the tar arrives in NAR order
    nar_byte_order("top-abc123", TREE, &mut flat);
    let tar_bytes = build_tar("top-abc123", &flat);

    let img = tmp.path().join("a");
    let wa = super::tar_to_image(
        tar_bytes.as_slice(),
        "top-abc123",
        "store-base",
        "volume",
        &img,
    )
    .await
    .unwrap();
    let (fa, _) = nar_to_erofs::finish_image(wa).await.unwrap();
    drop(fa);

    let sink_b = tokio::fs::File::create(tmp.path().join("b")).await.unwrap();
    let mut wb = nar_to_erofs::image_writer(sink_b, "volume").await.unwrap();
    let mut dec = nar_to_erofs::NarDecoder::new(HashReader::new(DirNar::new(&tree)));
    nar_to_erofs::write_nar(&mut dec, &mut wb, Some("store-base"))
        .await
        .unwrap();
    let (fb, _) = nar_to_erofs::finish_image(wb).await.unwrap();
    drop(fb);

    let a = std::fs::read(tmp.path().join("a")).unwrap();
    let b = std::fs::read(tmp.path().join("b")).unwrap();
    // the streaming path must reproduce the disk path byte for byte
    assert_eq!(a, b);
}
