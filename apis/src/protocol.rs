//! Constants and naming conventions shared by host and guest. Both sides
//! must agree on these for the virtio-fs image files, EROFS volume names,
//! and output-device labels to line up.

/// Offset of the output label within an output block device.
pub const OUTPUT_LABEL_OFFSET: u64 = 65536;
/// Prefix of the label written into an output block device; the rest is
/// the output name, e.g. `flaky-out:out`.
pub const OUTPUT_LABEL_PREFIX: &str = "flaky-out:";

/// The hash part of a store path (`/nix/store/<hash>-name` -> `<hash>`).
pub fn store_hash(store_path: &str) -> &str {
    let base = store_path
        .rsplit_once('/')
        .map(|(_, b)| b)
        .unwrap_or(store_path);
    base.split_once('-').map(|(h, _)| h).unwrap_or(base)
}

/// The virtio-fs filename for a store path's EROFS image.
pub fn image_name(store_path: &str) -> String {
    format!("{}.erofs", store_hash(store_path))
}

/// The EROFS volume name for a store path's image (first 16 hash chars).
pub fn volume_id(store_path: &str) -> String {
    store_hash(store_path)[..16].to_string()
}
