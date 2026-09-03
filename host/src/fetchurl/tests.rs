use super::*;

#[test]
fn nar_tokens_are_padded() {
    assert_eq!(nar_token("nix-archive-1").len(), 24);
    assert_eq!(nar_token("(").len(), 16);
    assert_eq!(nar_token("").len(), 8);
}

#[tokio::test]
async fn hashes_flat_and_recursive() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    let data = b"hello world"; // 11 bytes, pad 5
    tokio::fs::write(&p, data).await.unwrap();
    let flat = digest_of_file(&p, "flat", 11, false).await.unwrap();
    // reference: sha256("hello world")
    let want_flat: [u8; 32] = Sha256::digest(data).into();
    assert_eq!(flat, want_flat);

    let rec = digest_of_file(&p, "recursive", 11, false).await.unwrap();
    let mut h = Sha256::new();
    h.update(nar_prefix(11, false));
    h.update(data);
    h.update([0u8; 5]);
    h.update(nar_suffix());
    let want_rec: [u8; 32] = h.finalize().into();
    assert_eq!(rec, want_rec);
}

#[tokio::test]
async fn nar_of_file_roundtrips_through_decoder() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    let data = b"some file contents";
    tokio::fs::write(&p, data).await.unwrap();
    let reader = NarOfFile {
        file: tokio::fs::File::open(&p).await.unwrap(),
        prefix: std::io::Cursor::new(nar_prefix(data.len() as u64, false)),
        suffix: std::io::Cursor::new(nar_suffix()),
        size: data.len() as u64,
        read: 0,
        padded: false,
        state: 0,
    };
    let sink = std::io::Cursor::new(Vec::new());
    let sink = nar_to_erofs::nar_to_image(reader, sink, Some("abc-file"))
        .await
        .unwrap();
    let img = sink.into_inner();
    let magic = u32::from_le_bytes([img[1024], img[1025], img[1026], img[1027]]);
    assert_eq!(magic, 0xE0F5E1E2);
}
