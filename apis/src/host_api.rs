pub const HOST_API_PORT: u32 = 5001;

#[tarpc::service]
pub trait HostApi {
    async fn greet(x: String) -> String;
}
