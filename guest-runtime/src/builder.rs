use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom as StdSeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use apis::{BuildRequest, BuildResult, OutputImage};
use erofs_builder::{CreateOptions, DEFAULT_BLOCK_SIZE, InodeMeta, Writer};
use nix::mount::{MsFlags, mount};
use nix_drv::{generate_attrs_sh, structured_env};
use tokio::io::AsyncRead;

const IMAGE_DIR: &str = "/run/flaky-images";
const IMAGE_TAG: &str = "flaky-images";
const OUTPUT_LABEL_OFFSET: u64 = 65536;

struct Devices {
    outputs: BTreeMap<String, PathBuf>,
}

fn read_at(path: &Path, off: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = fs::File::open(path)?;
    f.seek(StdSeekFrom::Start(off))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn discover_devices() -> std::io::Result<Devices> {
    let mut devs = Devices {
        outputs: BTreeMap::new(),
    };
    let mut names: Vec<String> = fs::read_dir("/sys/block")?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("vd"))
        .collect();
    names.sort();
    for name in names {
        let path = PathBuf::from("/dev").join(&name);
        let label = read_at(&path, OUTPUT_LABEL_OFFSET, 64)?;
        let label = cstr(&label);
        if let Some(out) = label.strip_prefix("flaky-out:") {
            println!("guest: {name}: output device for output {out}");
            devs.outputs.insert(out.to_string(), path);
        } else {
            println!("guest: {name}: unrecognized device (label {label:?})");
        }
    }
    Ok(devs)
}

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
        println!("guest: symlink {store_path} -> {}", target.display());
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
    println!("guest: bind {} -> {}", src.display(), dst.display());
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
        let image =
            Path::new(IMAGE_DIR).join(format!("{}.erofs", nix_drv::hash_part(&inp.store_path)));
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

    let mut json: Option<serde_json::Value> = None;
    for (k, v) in &req.env {
        if k == "__json" {
            json = Some(serde_json::from_str(v).map_err(|e| {
                std::io::Error::other(format!("parsing __json for .attrs files: {e}"))
            })?);
        } else {
            env.push((k.clone(), v.clone()));
        }
    }
    if let Some(attrs) = &json {
        for (k, v) in structured_env(attrs) {
            set(&mut env, &k, &v);
        }
        let raw = req
            .env
            .iter()
            .find(|(k, _)| k == "__json")
            .unwrap()
            .1
            .clone();
        fs::write("/tmp/.attrs.json", &raw)?;
        fs::set_permissions("/tmp/.attrs.json", fs::Permissions::from_mode(0o644))?;
        let outs: Vec<(String, String)> = req
            .outputs
            .iter()
            .map(|o| (o.name.clone(), o.store_path.clone()))
            .collect();
        let sh = generate_attrs_sh(attrs, &outs);
        fs::write("/tmp/.attrs.sh", &sh)?;
        fs::set_permissions("/tmp/.attrs.sh", fs::Permissions::from_mode(0o644))?;
        println!(
            "guest: structured attrs: {} bytes json, {} bytes sh",
            raw.len(),
            sh.len()
        );
        set(&mut env, "NIX_ATTRS_JSON_FILE", "/tmp/.attrs.json");
        set(&mut env, "NIX_ATTRS_SH_FILE", "/tmp/.attrs.sh");
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
    let volume = &nix_drv::hash_part(store_path)[..16];
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)
        .await?;
    let sink = nar_to_erofs::CountingSink::new(file);
    let opts = CreateOptions {
        block_size: DEFAULT_BLOCK_SIZE,
        build_time: 0,
        build_time_nsec: 0,
        volume_name: volume.to_string(),
        ..Default::default()
    };
    let mut writer = Writer::new(sink, opts).await?;
    let base = nix_drv::basename(store_path);
    let path = Path::new(store_path);
    let md = fs::symlink_metadata(path)?;
    if md.is_dir() {
        pack_dir(&mut writer, path, base).await?;
    } else {
        // image root is a directory holding the file under its basename
        writer.mkdir(base, InodeMeta::dir(0o755)).await?;
        let mut meta = InodeMeta::reg((md.mode() & 0o7777) as u16);
        if md.mode() & 0o111 != 0 {
            meta.mode = 0o100555;
        }
        let mut r = SyncReader(fs::File::open(path)?);
        writer
            .add_file(&format!("{base}/{base}"), meta, md.len(), &mut r)
            .await?;
    }
    let sink = writer.finish().await?;
    let size = sink.count();
    let file = sink.into_inner();
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
    mount_image_files()?;
    let devices = discover_devices()?;
    for out in &req.outputs {
        if !devices.outputs.contains_key(&out.name) {
            return Err(std::io::Error::other(format!(
                "output device for {} not found",
                out.name
            )));
        }
    }

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
        let dev = &devices.outputs[&out.name];
        let size = pack_output(&out.store_path, dev).await?;
        println!(
            "guest: packed {} into {} ({} bytes)",
            out.store_path,
            dev.display(),
            size
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
