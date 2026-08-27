use std::ffi::CString;
use std::io::Write;

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

fn main() {
    setup_console();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "Hello from rust guest-init, pid 1!");
    let _ = writeln!(out, "__GUEST_EXIT__:0");
    let _ = out.flush();
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF as libc::c_int);
    }
    loop {
        std::thread::park();
    }
}
