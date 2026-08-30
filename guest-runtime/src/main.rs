use std::io::{self, Write};

use nix::fcntl::OFlag;
use nix::mount::MsFlags;

mod builder;
mod guest_api;
mod rpc;
mod session;

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

async fn serve() -> io::Result<()> {
    session::run_session().await
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
