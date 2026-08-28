pub use tarpc;
pub use tokio_serde;

mod postcard;

pub use postcard::Postcard;

#[tarpc::service]
pub trait VmController {
    async fn hello(x: String) -> String;
    async fn shutdown() -> String;
}
