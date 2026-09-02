use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::builder::{bind_from_image, mount_base, mount_fs, mount_image_files};

fn setup_nix(image: &str) -> std::io::Result<()> {
    mount_base()?;
    fs::create_dir_all("/etc")?;
    fs::write("/etc/passwd", "root:x:0:0:root:/root:/bin/sh\n")?;
    fs::write("/etc/group", "root:x:0:\n")?;
    fs::create_dir_all("/root")?;
    mount_image_files()?;
    fs::create_dir_all("/nix/store")?;
    fs::set_permissions("/nix/store", fs::Permissions::from_mode(0o755))?;
    fs::create_dir_all("/inputs")?;
    let src = Path::new("/run/flaky-images").join(image);
    let mnt = Path::new("/inputs").join("run-0");
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
    println!("guest: bound {count} entries from {image}");
    Ok(())
}

async fn run_nix(nix_root: &str, args: &[&str]) -> Result<String, String> {
    let nix = format!("{nix_root}/bin/nix");
    let out = tokio::process::Command::new(&nix)
        .args(args)
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
        .await
        .map_err(|e| format!("spawning nix: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    println!(
        "guest: nix {}: exited with {:?}, stdout {stdout:?}, stderr {stderr:?}",
        args[0],
        out.status.code()
    );
    match out.status.code() {
        Some(0) => Ok(stdout),
        code => Err(format!("nix {} exited with {code:?}: {stderr}", args[0])),
    }
}

pub async fn nix_version(image: String, nix_root: String) -> Result<String, String> {
    println!("guest: nix_version request (root {nix_root})");
    setup_nix(&image).map_err(|e| format!("setup: {e}"))?;
    run_nix(&nix_root, &["--version"]).await
}

pub async fn nix_eval(image: String, nix_root: String, expr: String) -> Result<String, String> {
    println!("guest: nix_eval request ({} bytes)", expr.len());
    setup_nix(&image).map_err(|e| format!("setup: {e}"))?;
    fs::create_dir_all("/tmp").map_err(|e| format!("setup: {e}"))?;
    fs::write("/tmp/expr.nix", &expr).map_err(|e| format!("setup: {e}"))?;
    run_nix(&nix_root, &["eval", "--file", "/tmp/expr.nix"]).await
}
