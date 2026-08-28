pub use tarpc;

#[tarpc::service]
pub trait VmController {
    async fn hello(x: String) -> String;
    async fn shutdown() -> String;
}
