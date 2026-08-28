use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use apis::tarpc::client::NewClient;
use apis::tarpc::context;
use apis::tarpc::server::{BaseChannel, Channel};
use apis::{GUEST_API_PORT, GuestApi, HOST_API_PORT, Postcard};
use futures::prelude::*;
use nix::fcntl::OFlag;
use nix::mount::MsFlags;
use tokio_vsock::VMADDR_CID_HOST;
use tokio_vsock::VsockAddr;
use tokio_vsock::VsockStream;

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
struct GuestApiServer {
    shutdown_requested: Arc<AtomicBool>,
}

impl GuestApi for GuestApiServer {
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

    let mut guest_api =
        Some(VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, GUEST_API_PORT)).await?);

    let host_api = VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, HOST_API_PORT)).await?;
    let _ = writeln!(
        out,
        "guest: connected to host on vsock port {HOST_API_PORT}"
    );
    let transport = apis::tarpc::serde_transport::Transport::from((host_api, Postcard::default()));
    let NewClient { client, dispatch } =
        apis::HostApiClient::new(apis::tarpc::client::Config::default(), transport);
    tokio::pin!(dispatch);
    let resp = tokio::select! {
        resp = client.greet(context::current(), "guest".to_string()) => {
            resp.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        }
        _ = &mut dispatch => {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "host api dispatch terminated"));
        }
    };
    let _ = writeln!(out, "guest: host replied: {resp}");

    loop {
        let stream = match guest_api.take() {
            Some(stream) => stream,
            None => VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, GUEST_API_PORT)).await?,
        };
        let _ = writeln!(
            out,
            "guest: connected to host on vsock port {GUEST_API_PORT}"
        );
        let transport =
            apis::tarpc::serde_transport::Transport::from((stream, Postcard::default()));
        let channel = BaseChannel::with_defaults(transport);
        let server = GuestApiServer {
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
    let _ = writeln!(out, "guest-runtime: pid {}", std::process::id());
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
