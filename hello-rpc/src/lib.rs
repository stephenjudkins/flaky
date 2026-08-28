pub use tarpc;

#[tarpc::service]
pub trait Hello {
    async fn hello(x: String) -> String;
}
