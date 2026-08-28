use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::prelude::*;
use nix::fcntl::OFlag;
use nix::mount::MsFlags;
use vm_controller_rpc::VmController;
use vm_controller_rpc::tarpc::context;
use vm_controller_rpc::tarpc::server::{BaseChannel, Channel};

mod vsock;

const VSOCK_PORT: u32 = 5000;

fn setup_console() {
    let _ = nix::mount::mount(
        None::<&str>,
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    );
    let _ = nix::mount::mount(
        None::<&str>,
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    );
    if let Ok(fd) = nix::fcntl::open("/dev/console", OFlag::O_RDWR, nix::sys::stat::Mode::empty()) {
        let _ = nix::unistd::dup2_stdin(&fd);
        let _ = nix::unistd::dup2_stdout(&fd);
        let _ = nix::unistd::dup2_stderr(&fd);
    }
}

fn poweroff() -> ! {
    nix::unistd::sync();
    let Err(e) = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
    let mut out = std::io::stdout();
    let _ = writeln!(out, "guest: poweroff failed: {e}");
    let _ = out.flush();
    loop {
        std::thread::park();
    }
}

#[derive(Clone)]
struct VmControllerServer {
    shutdown_requested: Arc<AtomicBool>,
}

impl VmController for VmControllerServer {
    async fn hello(self, _: context::Context, x: String) -> String {
        format!("hello {x}")
    }

    async fn shutdown(self, _: context::Context) -> String {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        "shutting down".to_string()
    }
}

async fn serve() -> std::io::Result<()> {
    let mut out = std::io::stdout();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    loop {
        let stream = vsock::VsockStream::connect(VSOCK_PORT, vsock::VMADDR_CID_HOST).await?;
        let _ = writeln!(out, "guest: connected to host on vsock port {VSOCK_PORT}");
        let transport = vm_controller_rpc::tarpc::serde_transport::Transport::from((
            stream,
            vm_controller_rpc::Postcard::default(),
        ));
        let channel = BaseChannel::with_defaults(transport);
        let server = VmControllerServer {
            shutdown_requested: shutdown_requested.clone(),
        };
        channel
            .execute(server.serve())
            .for_each(|resp| async {
                let _ = resp.await;
            })
            .await;
        let _ = writeln!(out, "guest: connection closed");
        if shutdown_requested.load(Ordering::SeqCst) {
            poweroff();
        }
    }
}

fn main() {
    setup_console();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "guest-init: pid {}", std::process::id());
    if let Err(e) = pid1::Pid1Settings::new().launch() {
        let _ = writeln!(out, "guest: pid1 launch failed: {e}");
        let _ = out.flush();
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let rc = match rt.block_on(serve()) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(out, "guest: rpc error: {e}");
            1
        }
    };
    let _ = writeln!(out, "__GUEST_EXIT__:{rc}");
    let _ = out.flush();
    poweroff();
}
