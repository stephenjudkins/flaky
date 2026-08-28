use std::ffi::CString;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::prelude::*;
use hello_rpc::Hello;
use hello_rpc::tarpc::context;
use hello_rpc::tarpc::server::{BaseChannel, Channel};

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
struct HelloServer;

impl Hello for HelloServer {
    async fn hello(self, _: context::Context, x: String) -> String {
        format!("hello {x}")
    }
}

async fn serve() -> std::io::Result<()> {
    let listener = vsock::VsockListener::bind(VSOCK_PORT)?;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "guest: listening on vsock port {VSOCK_PORT}");
    let stream = listener.accept().await?;
    let _ = writeln!(out, "guest: accepted host connection");
    let transport = hello_rpc::tarpc::serde_transport::Transport::from((
        stream,
        hello_rpc::tarpc::tokio_serde::formats::Json::default(),
    ));
    let answered = Arc::new(AtomicBool::new(false));
    let channel = BaseChannel::with_defaults(transport);
    let answered_done = answered.clone();
    let answered_done = answered.clone();
    tokio::select! {
        _ = channel.execute(HelloServer.serve()).for_each(move |resp| {
            let answered = answered_done.clone();
            async move {
                let _ = resp.await;
                answered.store(true, Ordering::SeqCst);
            }
        }) => (),
        _ = async {
            while !answered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } => (),
    }
    Ok(())
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
