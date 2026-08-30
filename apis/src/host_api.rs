pub const HOST_API_PORT: u32 = 5001;

#[tarpc::service]
pub trait HostApi {
    async fn greet(x: String) -> String;
    /// Forward a log line from the guest to the host console.
    async fn log(line: String) -> ();
}
