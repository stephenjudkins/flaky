use std::ffi::CString;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::prelude::*;
use vm_controller_rpc::VmController;
use vm_controller_rpc::tarpc::context;
use vm_controller_rpc::tarpc::server::{BaseChannel, Channel};

mod vsock;

const VSOCK_PORT: u32 = 5000;

fn setup_console() {
    unsafe {
        let none = CString::new("none").unwrap();
        let dev = CString::new("/dev").unwrap();
        let devtmpfs = CString::new("devtmpfs").unwrap();
        libc::mount(
            none.as_ptr(),
            dev.as_ptr(),
            devtmpfs.as_ptr(),
            0,
            std::ptr::null(),
        );
        let console = CString::new("/dev/console").unwrap();
        let fd = libc::open(console.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 {
                libc::close(fd);
            }
        }
    }
}

fn poweroff() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF as libc::c_int);
    }
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
        let stream = vsock::VsockStream::connect(VSOCK_PORT, libc::VMADDR_CID_HOST as u32).await?;
        let _ = writeln!(out, "guest: connected to host on vsock port {VSOCK_PORT}");
        let transport = vm_controller_rpc::tarpc::serde_transport::Transport::from((
            stream,
            vm_controller_rpc::tarpc::tokio_serde::formats::Json::default(),
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
    let _ = writeln!(out, "Hello from rust guest-init, pid 1!");
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
