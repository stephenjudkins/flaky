use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::builder::{
    IMAGE_DIR, bind_from_image, mount_base, mount_err, mount_fs, mount_image_files,
};

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
    match out.status.code() {
        Some(0) => {
            println!("guest: nix {}: ok", args[0]);
            Ok(stdout)
        }
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

pub async fn flake_eval(req: apis::FlakeEvalRequest) -> Result<String, String> {
    println!(
        "guest: flake_eval request (attr {}, {} inputs)",
        req.attr,
        req.inputs.len()
    );
    // repeated calls (one per attr) reuse the setup and mounts: the source
    // trees must stay at non-store paths, else nix tries to make the
    // read-only binds writable while copying them onto themselves
    if !Path::new("/nix/var/nix").exists() {
        setup_nix(&req.nix_image).map_err(|e| format!("setup: {e}"))?;
        fs::create_dir_all("/nix/var/nix").map_err(|e| format!("setup: {e}"))?;
        fs::create_dir_all("/tmp").map_err(|e| format!("setup: {e}"))?;
    }

    let mut overrides: Vec<(String, String)> = Vec::new();
    for inp in &req.inputs {
        let vol = apis::volume_id(&inp.store_path);
        let mnt = Path::new("/inputs").join(&vol);
        if !mnt.exists() {
            fs::create_dir_all(&mnt).map_err(|e| format!("setup: {e}"))?;
            mount_fs(&Path::new(IMAGE_DIR).join(&inp.image), &mnt, "erofs", true)
                .map_err(|e| format!("setup: {e}"))?;
        }
        overrides.push((
            inp.name.clone(),
            mnt.join(nix_drv::basename(&inp.store_path))
                .to_string_lossy()
                .into_owned(),
        ));
    }
    println!("guest: mounted {} flake inputs", overrides.len());

    if !Path::new("/flake/flake.nix").exists() {
        fs::create_dir_all("/flake").map_err(|e| format!("setup: {e}"))?;
        mount_err(
            nix::mount::mount(
                Some("flaky-src"),
                "/flake",
                Some("virtiofs"),
                nix::mount::MsFlags::MS_RDONLY
                    | nix::mount::MsFlags::MS_NODEV
                    | nix::mount::MsFlags::MS_NOSUID,
                None::<&str>,
            ),
            "mount virtiofs flaky-src on /flake",
        )
        .map_err(|e| format!("setup: {e}"))?;
    }

    let mut args: Vec<String> = vec![
        "--offline".into(),
        "derivation".into(),
        "show".into(),
        "-r".into(),
    ];
    for (name, path) in &overrides {
        args.push("--override-input".into());
        args.push(name.clone());
        args.push(path.clone());
    }
    args.push(format!("/flake#{}", req.attr));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_nix(&req.nix_root, &arg_refs).await
}

pub async fn shell(req: apis::ShellRequest) -> Result<i32, String> {
    let setup = || -> std::io::Result<()> {
        mount_base()?;
        fs::create_dir_all("/nix/store")?;
        fs::set_permissions("/nix/store", fs::Permissions::from_mode(0o755))?;
        fs::create_dir_all("/inputs")?;
        fs::create_dir_all("/root")?;
        fs::create_dir_all("/tmp")?;
        mount_image_files()?;
        for inp in &req.inputs {
            let image = Path::new(IMAGE_DIR).join(apis::image_name(&inp.store_path));
            let mnt = Path::new("/inputs").join(&inp.volume_id);
            fs::create_dir_all(&mnt)?;
            mount_fs(&image, &mnt, "erofs", true)?;
            bind_from_image(&mnt, &inp.store_path)?;
        }
        Ok(())
    };
    setup().map_err(|e| format!("setup: {e}"))?;

    let mut cmd = tokio::process::Command::new(&req.shell);
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .env_clear()
        .env("PATH", req.path.join(":"))
        .env("HOME", "/root")
        .env("TERM", &req.term)
        .current_dir("/");
    // the session loop keeps serving RPCs; the shell shares the console.
    // New session + controlling tty so job control (Ctrl-C) works.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0 as *mut libc::c_void) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = cmd
        .status()
        .await
        .map_err(|e| format!("spawning shell: {e}"))?;
    Ok(status.code().unwrap_or(1))
}
