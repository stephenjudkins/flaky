use apis::HostApi;
use apis::tarpc::context;

#[derive(Clone)]
pub struct HostApiServer;

impl HostApi for HostApiServer {
    async fn greet(self, _: context::Context, x: String) -> String {
        format!("hello from host: {x}")
    }

    async fn log(self, _: context::Context, line: String) -> () {
        eprintln!("[guest] {line}");
    }
}
