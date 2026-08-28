pub const GUEST_API_PORT: u32 = 5000;

#[tarpc::service]
pub trait GuestApi {
    async fn hello(x: String) -> String;
    async fn shutdown() -> String;
}
