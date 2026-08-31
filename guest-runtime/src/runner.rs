use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nix::mount::MsFlags;

use crate::builder::{bind_from_image, mount_err, mount_fs, mount_image_files};

fn base_mounts() -> std::io::Result<()> {
    fs::create_dir_all("/etc")?;
    fs::write("/etc/passwd", "root:x:0:0:root:/root:/bin/sh\n")?;
    fs::write("/etc/group", "root:x:0:\n")?;
    fs::create_dir_all("/root")?;
    let _ = mount_err(
        nix::mount::mount(
            None::<&str>,
            "/sys",
            Some("sysfs"),
            MsFlags::empty(),
            None::<&str>,
        ),
        "mount sysfs",
    );
    let _ = mount_err(
        nix::mount::mount(
            None::<&str>,
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        ),
        "mount proc",
    );
    let _ = std::os::unix::fs::symlink("/proc/self/fd", "/dev/fd");
    let _ = std::os::unix::fs::symlink("/proc/self/fd/0", "/dev/stdin");
    let _ = std::os::unix::fs::symlink("/proc/self/fd/1", "/dev/stdout");
    let _ = std::os::unix::fs::symlink("/proc/self/fd/2", "/dev/stderr");
    Ok(())
}

/// Mounts one closure image read-only and binds every top-level entry
/// (a store path basename) into /nix/store.
fn mount_closure_image(image: &str, idx: usize) -> std::io::Result<()> {
    let src = Path::new("/run/flaky-images").join(image);
    let mnt = Path::new("/inputs").join(format!("run-{idx}"));
    fs::create_dir_all(&mnt)?;
    mount_fs(&src, &mnt, "erofs", true)?;
    let mut entries: Vec<String> = fs::read_dir(&mnt)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    let count = entries.len();
    for name in entries {
        let store_path = format!("/nix/store/{name}");
        bind_from_image(&mnt, &store_path)?;
    }
    println!("guest: nix_version: bound {count} entries from {image}");
    Ok(())
}

fn setup_nix(image: &str) -> std::io::Result<()> {
    base_mounts()?;
    mount_image_files()?;
    fs::create_dir_all("/nix/store")?;
    fs::set_permissions("/nix/store", fs::Permissions::from_mode(0o755))?;
    fs::create_dir_all("/inputs")?;
    mount_closure_image(image, 0)
}

pub async fn nix_version(image: String, nix_root: String) -> std::io::Result<String> {
    println!("guest: nix_version request (root {nix_root})");
    setup_nix(&image)?;

    let nix = format!("{nix_root}/bin/nix");
    let out = tokio::process::Command::new(&nix)
        .arg("--version")
        .env_clear()
        .env("NIX_STORE", "/nix/store")
        .env("HOME", "/root")
        .current_dir("/tmp")
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    println!(
        "guest: nix_version: exited with {:?}, stdout {stdout:?}, stderr {stderr:?}",
        out.status.code()
    );
    match out.status.code() {
        Some(0) => Ok(stdout),
        code => Err(std::io::Error::other(format!(
            "nix --version exited with {code:?}: {stderr}"
        ))),
    }
}

pub async fn nix_eval(image: String, nix_root: String, expr: String) -> std::io::Result<String> {
    println!("guest: nix_eval request ({} bytes)", expr.len());
    setup_nix(&image)?;
    fs::create_dir_all("/tmp")?;
    fs::write("/tmp/expr.nix", &expr)?;

    let nix = format!("{nix_root}/bin/nix");
    let out = tokio::process::Command::new(&nix)
        .args(["eval", "--file", "/tmp/expr.nix"])
        .env_clear()
        .env("NIX_STORE", "/nix/store")
        .env("HOME", "/root")
        .env("PATH", format!("{nix_root}/bin:/bin"))
        .env(
            "NIX_CONFIG",
            "extra-experimental-features = nix-command flakes\n",
        )
        .current_dir("/tmp")
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    println!(
        "guest: nix_eval: exited with {:?}, stdout {stdout:?}, stderr {stderr:?}",
        out.status.code()
    );
    match out.status.code() {
        Some(0) => Ok(stdout),
        code => Err(std::io::Error::other(format!(
            "nix eval exited with {code:?}: {stderr}"
        ))),
    }
}
