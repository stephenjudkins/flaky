use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use apis::GuestApi;
use apis::tarpc::context;

#[derive(Clone)]
pub struct GuestApiServer {
    shutdown_requested: Arc<AtomicBool>,
}

impl GuestApiServer {
    pub fn new() -> Self {
        GuestApiServer {
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }
}

impl GuestApi for GuestApiServer {
    async fn hello(self, _: context::Context, x: String) -> String {
        format!("hello {x}")
    }

    async fn build(self, _: context::Context, request: apis::BuildRequest) -> apis::BuildResult {
        crate::builder::run_build(request).await
    }

    async fn nix_version(self, _: context::Context, image: String, nix_root: String) -> String {
        match crate::runner::nix_version(image, nix_root).await {
            Ok(v) => v,
            Err(e) => format!("guest: nix_version error: {e}"),
        }
    }

    async fn nix_eval(
        self,
        _: context::Context,
        image: String,
        nix_root: String,
        expr: String,
    ) -> String {
        match crate::runner::nix_eval(image, nix_root, expr).await {
            Ok(v) => v,
            Err(e) => format!("guest: nix_eval error: {e}"),
        }
    }

    async fn shutdown(self, _: context::Context) -> String {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        "shutting down".to_string()
    }
}
