use std::fs;
use std::os::unix::fs::PermissionsExt;

use tempfile::TempDir;

use super::DirNar;
use crate::nar::HashReader;

#[tokio::test]
async fn round_trips_through_nar_decoder() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("sub/deep")).unwrap();
    fs::write(root.join("sub/deep/b.txt"), b"world").unwrap();
    fs::write(root.join("a.txt"), b"hello").unwrap();
    fs::write(root.join("empty.txt"), b"").unwrap();
    fs::create_dir_all(root.join("empty-dir")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    let exec = root.join("run.sh");
    fs::write(&exec, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).unwrap();

    let reader = HashReader::new(DirNar::new(root));
    let mut decoder = nar_to_erofs::NarDecoder::new(reader);
    let mut entries = Vec::new();
    while let Some(e) = decoder.next_entry().await.unwrap() {
        use nar_to_erofs::EntryKind::*;
        let kind = match e.kind {
            Directory => "dir".to_string(),
            Regular { executable, size } => format!("file exec={executable} size={size}"),
            Symlink { target } => format!("link -> {target}"),
        };
        entries.push((e.path.clone(), kind));
    }
    let digest = decoder.into_inner().digest();
    assert_eq!(digest.len(), 32);

    let expected = [
        ("", "dir"),
        ("a.txt", "file exec=false size=5"),
        ("empty-dir", "dir"),
        ("empty.txt", "file exec=false size=0"),
        ("link", "link -> a.txt"),
        ("run.sh", "file exec=true size=10"),
        ("sub", "dir"),
        ("sub/deep", "dir"),
        ("sub/deep/b.txt", "file exec=false size=5"),
    ];
    for (path, kind) in expected {
        assert!(
            entries.iter().any(|(p, k)| p == path && k == kind),
            "missing {path:?} ({kind}) in {entries:?}"
        );
    }
    assert_eq!(entries.len(), expected.len());
}
