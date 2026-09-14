use super::*;

#[test]
fn store_path_hash_validates() {
    assert!(StorePathHash::new("ki4g5pbmqcs07mwrxmvik6dpnzyx3z4c").is_ok());
    assert!(StorePathHash::new("ki4g5pbmqcs07mwrxmvik6dpnzyx3z4e").is_err()); // 'e' not in alphabet
    assert!(StorePathHash::new("short").is_err());
    assert_eq!(
        StorePathHash::from_store_path("/nix/store/846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3")
            .unwrap()
            .as_str(),
        "846h582z2d4mifn4km7axlqllcyn6zdg"
    );
    assert!(StorePathHash::from_store_path("/some/other/path").is_err());
}

fn disk_cache() -> (NixCache, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cache = NixCache::new("https://cache.nixos.org")
        .unwrap()
        .with_disk_cache(dir.path());
    (cache, dir)
}

const NARINFO: &str = "StorePath: /nix/store/846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3\nURL: nar/846h582z2d4mifn4km7axlqllcyn6zdg.nar.xz\nCompression: xz\nFileHash: sha256-tQjFHAWTPDNBSqyI2DWfxFOh7MjcUWTOk+frQdpatkc=\nFileSize: 196040\nNarHash: sha256-F+zLLMvODPI7SRDEKQG7i7KOJc7m0S9HITrFhUiVRNA=\nNarSize: 741000\nReferences: abcdefghijklmnopqrstuvwx12345678-name\nDeriver: 7pgxjakchwmnbjvkqrym0sqjw29jpgsa-hello-2.12.3.drv\nSystem: aarch64-linux\nSig: cache.nixos.org-1:fake\n";

#[tokio::test]
async fn disk_cache_serves_hits_without_network() {
    let (cache, dir) = disk_cache();
    let hash = StorePathHash::new("846h582z2d4mifn4km7axlqllcyn6zdg").unwrap();
    cache.write_local(&hash, NARINFO);
    let Lookup::Hit(nar) = cache.lookup(&hash).await.unwrap() else {
        panic!("expected hit");
    };
    assert_eq!(nar.url, "nar/846h582z2d4mifn4km7axlqllcyn6zdg.nar.xz");
    assert_eq!(nar.nar_size, 741000);
    assert_eq!(
        nar.references,
        vec!["abcdefghijklmnopqrstuvwx12345678-name"]
    );
    assert!(dir.path().join("cache.nixos.org").exists());
}

#[test]
fn parses_nix_base32_nar_hash() {
    // real cache.nixos.org narinfo format
    let text = "StorePath: /nix/store/846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3
URL: nar/1akpk014pgd44rxrdcrp92jhd1li209xp1i4nhxvsfndm1d0hnz1.nar.zst
Compression: zstd
FileHash: sha256:0hhp7f3y1g8jfmqp3v30wrqryq3rsc5kzjjzljc81gxxflmqm4fg
FileSize: 77097
NarHash: sha256:1akpk014pgd44rxrdcrp92jhd1li209xp1i4nhxvsfndm1d0hnz1
NarSize: 294440
References: 846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3
Deriver: lzg8d4i5vkiqi0f087klds3819125bd3-hello-2.12.3.drv
System: aarch64-linux
Sig: cache.nixos.org-1:fake
";
    let nar = parse_narinfo(text).unwrap();
    assert_eq!(
        nix_drv::nix_base32_encode(&nar.nar_hash),
        "1akpk014pgd44rxrdcrp92jhd1li209xp1i4nhxvsfndm1d0hnz1"
    );
}

#[test]
fn legacy_miss_markers_are_removed() {
    let (cache, _dir) = disk_cache();
    let hash = StorePathHash::new("846h582z2d4mifn4km7axlqllcyn6zdg").unwrap();
    cache.write_local(&hash, "miss 12345\n");
    assert!(cache.lookup_local(&hash).unwrap().is_none());
}

#[test]
fn corrupt_entries_are_discarded() {
    let (cache, _dir) = disk_cache();
    let hash = StorePathHash::new("846h582z2d4mifn4km7axlqllcyn6zdg").unwrap();
    cache.write_local(&hash, "garbage: yes\n");
    assert!(cache.lookup_local(&hash).unwrap().is_none());
}

#[test]
fn narinfo_parses() {
    let text = "\
StorePath: /nix/store/846h582z2d4mifn4km7axlqllcyn6zdg-hello-2.12.3
URL: nar/846h582z2d4mifn4km7axlqllcyn6zdg.nar.xz
Compression: xz
FileHash: sha256-tQjFHAWTPDNBSqyI2DWfxFOh7MjcUWTOk+frQdpatkc=
FileSize: 196040
NarHash: sha256-F+zLLMvODPI7SRDEKQG7i7KOJc7m0S9HITrFhUiVRNA=
NarSize: 741000
References: abcdefghijklmnopqrstuvwx12345678-name other-name
Deriver: 7pgxjakchwmnbjvkqrym0sqjw29jpgsa-hello-2.12.3.drv
System: aarch64-linux
Sig: cache.nixos.org-1:fake";
    let info = NarInfo::parse(text).map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(
        info.references,
        vec![
            std::borrow::Cow::from("abcdefghijklmnopqrstuvwx12345678-name"),
            std::borrow::Cow::from("other-name")
        ]
    );
    assert_eq!(info.url, "nar/846h582z2d4mifn4km7axlqllcyn6zdg.nar.xz");
    assert_eq!(
        Compression::from_narinfo(info.compression.as_deref(), info.url),
        Compression::Xz
    );
    assert_eq!(
        Compression::from_narinfo(None, "nar/abc.nar.xz"),
        Compression::Xz
    );
    assert_eq!(
        Compression::from_narinfo(None, "nar/abc.nar"),
        Compression::None
    );
}

/// Live test against cache.nixos.org; run with
/// `cargo test -p nix-cache -- --ignored` when online.
#[tokio::test]
#[ignore]
async fn fetches_nar_from_cache_nixos_org() {
    // /nix/store/wj7phsmi7ncidl8k00p489krqss7n9sd-hello-2.12.3.tar.gz
    let cache = NixCache::new("https://cache.nixos.org").unwrap();
    let hash = StorePathHash::new("wj7phsmi7ncidl8k00p489krqss7n9sd").unwrap();
    let Lookup::Hit(nar) = cache.lookup(&hash).await.unwrap() else {
        panic!("expected cache hit");
    };
    assert!(matches!(nar.compression, Compression::Zstd));
    let sink = std::io::Cursor::new(Vec::new());
    let sink = fetch_nar_to_erofs(
        &cache,
        &nar,
        sink,
        Some("wj7phsmi7ncidl8k00p489krqss7n9sd-hello-2.12.3.tar.gz"),
    )
    .await
    .unwrap();
    let img = sink.into_inner();
    let magic = u32::from_le_bytes([img[1024], img[1025], img[1026], img[1027]]);
    assert_eq!(magic, 0xE0F5E1E2, "erofs magic");
    assert!(img.len() > 1024 + 128, "image has content");
}
