use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use apis::{BuildRequest, BuildResult, OutputImage};
use erofs_builder::{InodeMeta, Writer};
use nix::mount::{MsFlags, mount};
use nix_drv::{generate_attrs_sh, structured_env};
use tokio::io::AsyncRead;

pub(crate) const IMAGE_DIR: &str = "/run/flaky-images";
const IMAGE_TAG: &str = "flaky-images";

pub(crate) fn mount_err<T>(r: nix::Result<T>, what: &str) -> std::io::Result<T> {
    r.map_err(|e| std::io::Error::other(format!("{what}: {e}")))
}

pub(crate) fn mount_fs(src: &Path, dst: &Path, fstype: &str, ro: bool) -> std::io::Result<()> {
    let flags = if ro {
        MsFlags::MS_RDONLY
    } else {
        MsFlags::empty()
    };
    mount_err(
        mount(
            Some(src.to_str().unwrap()),
            dst.to_str().unwrap(),
            Some(fstype),
            flags,
            None::<&str>,
        ),
        &format!("mount {fstype} {} on {}", src.display(), dst.display()),
    )
}

pub(crate) fn mount_image_files() -> std::io::Result<()> {
    fs::create_dir_all(IMAGE_DIR)?;
    fs::set_permissions(IMAGE_DIR, fs::Permissions::from_mode(0o700))?;
    mount_err(
        mount(
            Some(IMAGE_TAG),
            IMAGE_DIR,
            Some("virtiofs"),
            MsFlags::MS_RDONLY | MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            None::<&str>,
        ),
        &format!("mount virtiofs {IMAGE_TAG} on {IMAGE_DIR}"),
    )
}

/// Best-effort base mounts shared by every guest task: sysfs, proc, and
/// the /dev/fd|stdin|stdout|stderr symlinks.
pub(crate) fn mount_base() -> std::io::Result<()> {
    let _ = mount_err(
        mount(
            None::<&str>,
            "/sys",
            Some("sysfs"),
            MsFlags::empty(),
            None::<&str>,
        ),
        "mount sysfs",
    );
    let _ = mount_err(
        mount(
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

fn mount_bind(src: &Path, dst: &Path) -> std::io::Result<()> {
    mount_err(
        mount(
            Some(src.to_str().unwrap()),
            dst.to_str().unwrap(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        ),
        &format!("bind {} on {}", src.display(), dst.display()),
    )
}

/// Binds `<image_root>/<basename of store_path>` over `store_path`.
pub(crate) fn bind_from_image(image_root: &Path, store_path: &str) -> std::io::Result<()> {
    let base = nix_drv::basename(store_path);
    let src = image_root.join(base);
    let dst = Path::new(store_path);
    let md = fs::symlink_metadata(&src)?;
    if md.file_type().is_symlink() {
        let target = fs::read_link(&src)?;
        let _ = fs::remove_file(dst);
        std::os::unix::fs::symlink(&target, dst)?;
        return Ok(());
    }
    if md.is_dir() {
        fs::create_dir_all(dst)?;
    } else if fs::symlink_metadata(dst).is_err() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(dst)?;
    }
    mount_bind(&src, dst)
}

fn setup_mounts(req: &BuildRequest) -> std::io::Result<()> {
    fs::create_dir_all("/nix/store")?;
    fs::set_permissions("/nix/store", fs::Permissions::from_mode(0o755))?;
    fs::create_dir_all("/tmp")?;
    mount_err(
        mount(
            Some("tmpfs"),
            "/tmp",
            Some("tmpfs"),
            MsFlags::empty(),
            Some("mode=0755"),
        ),
        "mount tmpfs on /tmp",
    )?;
    fs::create_dir_all("/homeless-shelter")?;
    fs::set_permissions("/homeless-shelter", fs::Permissions::from_mode(0o755))?;

    if !req.inputs.is_empty() {
        fs::create_dir_all("/inputs")?;
    }
    for (i, inp) in req.inputs.iter().enumerate() {
        let image = Path::new(IMAGE_DIR).join(apis::image_name(&inp.store_path));
        let mnt = Path::new("/inputs").join(&inp.volume_id);
        fs::create_dir_all(&mnt)?;
        mount_fs(&image, &mnt, "erofs", true)?;
        if (i + 1) % 16 == 0 {
            println!("guest: mounted {}/{} inputs", i + 1, req.inputs.len());
        }
        bind_from_image(&mnt, &inp.store_path)?;
    }
    println!("guest: all {} inputs mounted", req.inputs.len());

    // outputs are deliberately NOT pre-created: like real nix, builders
    // must mkdir (or write a file at) $out themselves, and pack_output
    // handles both directory and file outputs
    Ok(())
}

/// Builds the process environment for the builder and, for structured
/// attrs, writes `.attrs.json` / `.attrs.sh` into /tmp.
fn prepare_env(req: &BuildRequest) -> std::io::Result<Vec<(String, String)>> {
    fn set(env: &mut Vec<(String, String)>, k: &str, v: &str) {
        if let Some(e) = env.iter_mut().find(|(ek, _)| ek == k) {
            e.1 = v.to_string();
        } else {
            env.push((k.to_string(), v.to_string()));
        }
    }

    let mut env: Vec<(String, String)> = Vec::new();

    let mut attrs: Option<serde_json::Value> = None;
    for (k, v) in &req.env {
        if k == "__json" {
            attrs = Some(serde_json::from_str(v).map_err(|e| {
                std::io::Error::other(format!("parsing __json for .attrs files: {e}"))
            })?);
        } else {
            env.push((k.clone(), v.clone()));
        }
    }
    if attrs.is_none() {
        if let Some(sa) = &req.structured_attrs {
            attrs =
                Some(serde_json::from_str(sa).map_err(|e| {
                    std::io::Error::other(format!("parsing structured attrs: {e}"))
                })?);
        }
    }
    if let Some(attrsv) = &attrs {
        for (k, v) in structured_env(attrsv) {
            set(&mut env, &k, &v);
        }
        fs::write(
            "/tmp/.attrs.json",
            serde_json::to_string(attrsv).unwrap_or_default(),
        )?;
        fs::set_permissions("/tmp/.attrs.json", fs::Permissions::from_mode(0o644))?;
        let outs: Vec<(String, String)> = req
            .outputs
            .iter()
            .map(|o| (o.name.clone(), o.store_path.clone()))
            .collect();
        let sh = generate_attrs_sh(attrsv, &outs);
        fs::write("/tmp/.attrs.sh", &sh)?;
        fs::set_permissions("/tmp/.attrs.sh", fs::Permissions::from_mode(0o644))?;
        set(&mut env, "NIX_ATTRS_JSON_FILE", "/tmp/.attrs.json");
        set(&mut env, "NIX_ATTRS_SH_FILE", "/tmp/.attrs.sh");
    }

    // replicate nix's passAsFile handling: named vars are removed from the
    // environment, written to /tmp/.attr-<name>, and <name>Path points at
    // the file
    let pass: Option<String> = env
        .iter()
        .find(|(k, _)| k == "passAsFile")
        .map(|(_, v)| v.clone())
        .or_else(|| {
            attrs
                .as_ref()
                .and_then(|a| a.get("passAsFile"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    if let Some(pass) = pass {
        for name in pass.split_whitespace() {
            let value = env
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            env.retain(|(k, _)| k != name);
            let file = format!("/tmp/.attr-{}", name.replace(':', "-"));
            fs::write(&file, value.as_bytes())?;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644))?;
            set(&mut env, &format!("{name}Path"), &file);
        }
    }
    let defaults: &[(&str, &str)] = &[
        ("NIX_STORE", "/nix/store"),
        ("NIX_BUILD_TOP", "/tmp"),
        ("TMPDIR", "/tmp"),
        ("TEMPDIR", "/tmp"),
        ("TMP", "/tmp"),
        ("HOME", "/homeless-shelter"),
        ("NIX_BUILD_CORES", "2"),
    ];
    for (k, v) in defaults {
        if !env.iter().any(|(ek, _)| ek == *k) {
            env.push((k.to_string(), v.to_string()));
        }
    }
    Ok(env)
}

struct SyncReader<R>(R);

impl<R: Read + Unpin> AsyncRead for SyncReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            match this.0.read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

fn meta_from_mode(st_mode: u32) -> InodeMeta {
    InodeMeta {
        mode: (st_mode & 0o170000) as u16 | ((st_mode & 0o7777) as u16),
        ..Default::default()
    }
}

async fn pack_dir<W>(writer: &mut Writer<W>, dir: &Path, prefix: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + tokio::io::AsyncSeek + Unpin + Send,
{
    let md = fs::symlink_metadata(dir)?;
    let meta = InodeMeta::dir((md.mode() & 0o7777) as u16);
    writer.mkdir(prefix, meta).await?;
    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel = format!("{prefix}/{name}");
        let md = fs::symlink_metadata(&path)?;
        let ft = md.file_type();
        if ft.is_dir() {
            Box::pin(pack_dir(writer, &path, &rel)).await?;
        } else if ft.is_file() {
            let mut meta = InodeMeta::reg((md.mode() & 0o7777) as u16);
            if md.mode() & 0o111 != 0 {
                meta.mode = 0o100555;
            }
            let f = fs::File::open(&path)?;
            let mut r = SyncReader(f);
            writer.add_file(&rel, meta, md.len(), &mut r).await?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&path)?;
            writer
                .symlink(
                    &rel,
                    target.as_os_str().as_encoded_bytes(),
                    InodeMeta::symlink(),
                )
                .await?;
        } else {
            let mut meta = meta_from_mode(md.mode());
            meta.rdev = md.rdev() as u32;
            writer.mknod(&rel, meta).await?;
        }
    }
    Ok(())
}

async fn pack_output(store_path: &str, dev: &Path) -> std::io::Result<u64> {
    let volume = apis::volume_id(store_path);
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)
        .await?;
    let mut writer = nar_to_erofs::image_writer(file, &volume)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let base = nix_drv::basename(store_path);
    let path = Path::new(store_path);
    let md = fs::symlink_metadata(path)?;
    if md.is_dir() {
        pack_dir(&mut writer, path, base).await?;
    } else if md.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        writer
            .symlink(
                base,
                target.as_os_str().as_encoded_bytes(),
                InodeMeta::symlink(),
            )
            .await?;
    } else {
        let mut meta = InodeMeta::reg((md.mode() & 0o7777) as u16);
        if md.mode() & 0o111 != 0 {
            meta.mode = 0o100555;
        }
        let mut r = SyncReader(fs::File::open(path)?);
        writer.add_file(base, meta, md.len(), &mut r).await?;
    }
    let (file, size) = nar_to_erofs::finish_image(writer)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    file.sync_all().await?;
    Ok(size)
}

/// Packs the given store paths as top-level entries of one erofs image
/// onto `dev` (paths created by nix during flake eval, passed back to the
/// host as build inputs).
pub async fn pack_store_paths(paths: &[String], dev: &Path) -> std::io::Result<u64> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)
        .await?;
    let mut writer = nar_to_erofs::image_writer(file, "flake-inputs")
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    for p in paths {
        let base = nix_drv::basename(p);
        let path = Path::new(p);
        let md = fs::symlink_metadata(path)?;
        if md.is_dir() {
            pack_dir(&mut writer, path, base).await?;
        } else if md.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            writer
                .symlink(
                    base,
                    target.as_os_str().as_encoded_bytes(),
                    InodeMeta::symlink(),
                )
                .await?;
        } else {
            let mut meta = InodeMeta::reg((md.mode() & 0o7777) as u16);
            if md.mode() & 0o111 != 0 {
                meta.mode = 0o100555;
            }
            let mut r = SyncReader(fs::File::open(path)?);
            writer.add_file(base, meta, md.len(), &mut r).await?;
        }
    }
    let (file, size) = nar_to_erofs::finish_image(writer)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    file.sync_all().await?;
    Ok(size)
}

fn dump_config_logs(dir: &Path) {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                visit(&p, out);
            } else if e.file_name() == "config.log" {
                out.push(p);
            }
        }
    }
    let mut logs = Vec::new();
    visit(dir, &mut logs);
    for log in logs {
        println!("guest: ===== {} =====", log.display());
        if let Ok(text) = fs::read_to_string(&log) {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines
                .iter()
                .rposition(|l| l.contains("whether the C compiler works"))
                .map(|i| i.saturating_sub(30))
                .unwrap_or_else(|| lines.len().saturating_sub(400));
            for l in &lines[start..] {
                println!("guest: | {l}");
            }
        }
    }
}

pub async fn run_build(req: BuildRequest) -> BuildResult {
    match run_build_inner(req).await {
        Ok(r) => r,
        Err(e) => {
            println!("guest: build error: {e}");
            BuildResult {
                success: false,
                exit_code: None,
                error: Some(format!("{e}")),
                outputs: vec![],
            }
        }
    }
}

async fn run_build_inner(req: BuildRequest) -> std::io::Result<BuildResult> {
    println!(
        "guest: build request for {} (builder {}, system outputs: {:?})",
        req.drv_path,
        req.builder,
        req.outputs
            .iter()
            .map(|o| o.name.clone())
            .collect::<Vec<_>>()
    );
    mount_base()?;
    mount_image_files()?;
    setup_mounts(&req)?;
    let env = prepare_env(&req)?;

    unsafe { libc::umask(0o022) };
    println!("guest: spawning builder: {} {:?}", req.builder, req.args);
    let mut cmd = tokio::process::Command::new(&req.builder);
    cmd.args(&req.args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir("/tmp");
    let status = cmd.status().await?;
    println!("guest: builder exited: {status}");
    if !status.success() {
        dump_config_logs(Path::new("/tmp"));
        return Ok(BuildResult {
            success: false,
            exit_code: status.code(),
            error: Some(format!("builder exited with {status}")),
            outputs: vec![],
        });
    }

    let mut outputs = Vec::new();
    for out in &req.outputs {
        let size = pack_output(&out.store_path, Path::new(&out.device)).await?;
        println!(
            "guest: packed {} into {} ({} bytes)",
            out.store_path, out.device, size
        );
        outputs.push(OutputImage {
            name: out.name.clone(),
            store_path: out.store_path.clone(),
            image_size: size,
        });
    }
    Ok(BuildResult {
        success: true,
        exit_code: status.code(),
        error: None,
        outputs,
    })
}
