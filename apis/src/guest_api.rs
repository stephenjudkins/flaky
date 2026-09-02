pub const GUEST_API_PORT: u32 = 5000;

use serde::{Deserialize, Serialize};

#[tarpc::service]
pub trait GuestApi {
    async fn build(request: BuildRequest) -> BuildResult;
    async fn nix_version(image: String, nix_root: String) -> Result<String, String>;
    async fn nix_eval(image: String, nix_root: String, expr: String) -> Result<String, String>;
    async fn shutdown() -> String;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    pub drv_path: String,
    pub builder: String,
    pub args: Vec<String>,
    /// The drv env; for structured attrs this contains `__json`.
    pub env: Vec<(String, String)>,
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
