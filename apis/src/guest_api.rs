pub const GUEST_API_PORT: u32 = 5000;

use serde::{Deserialize, Serialize};

#[tarpc::service]
pub trait GuestApi {
    async fn build(request: BuildRequest) -> BuildResult;
    async fn nix_version(image: String, nix_root: String) -> Result<String, String>;
    async fn nix_eval(image: String, nix_root: String, expr: String) -> Result<String, String>;
    async fn flake_eval(request: FlakeEvalRequest) -> Result<String, String>;
    async fn pack_store_paths(request: PackPathsRequest) -> Result<u64, String>;
    async fn shutdown() -> String;
}

/// One flake input: the erofs image file name (inside the virtio-fs image
/// dir) and the store path it holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeInputSpec {
    pub name: String,
    pub image: String,
    pub store_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeEvalRequest {
    pub nix_image: String,
    pub nix_root: String,
    pub attr: String,
    pub inputs: Vec<FlakeInputSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackPathsRequest {
    pub paths: Vec<String>,
    pub device: String,
}

/// How a store path is provided to the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSpec {
    /// Absolute store path, e.g. /nix/store/abc-name.
    pub store_path: String,
    /// Identity used to find the EROFS volume: first 16 chars of the store
    /// path hash (the image's erofs volume_name).
    pub volume_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    /// Output name, e.g. "out", "dev".
    pub name: String,
    /// Absolute store path of this output.
    pub store_path: String,
    /// Guest block device node backing this output, e.g. "/dev/vda". The
    /// host attaches output devices to the VM in `outputs` order and the
    /// kernel enumerates virtio-blk devices in attachment order.
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    pub drv_path: String,
    pub builder: String,
    pub args: Vec<String>,
    /// The drv env; for structured attrs this contains `__json`.
    pub env: Vec<(String, String)>,
    /// v4 derivation format: the typed structured attrs JSON, when present.
    pub structured_attrs: Option<String>,
    pub outputs: Vec<OutputSpec>,
    /// Store paths provided as their own erofs block devices, one device
    /// per path, identified by volume id.
    pub inputs: Vec<InputSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputImage {
    pub name: String,
    pub store_path: String,
    /// Bytes written to the output block device.
    pub image_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub outputs: Vec<OutputImage>,
}
