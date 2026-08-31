use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod fetchurl;
mod host_api;
mod image_fs;
mod orchestrator;
mod rpc;
mod session;
mod vm;
mod vsock_device;
use host_api::HostApiServer;
use vsock_device::{VsockHost, VsockParam};

async fn run_session(vsock_host: &VsockHost) -> anyhow::Result<()> {
    let guest_api_stream =
        tokio::net::UnixStream::from_std(vsock_host.accept(apis::GUEST_API_PORT).await?)?;
    let host_api_stream =
        tokio::net::UnixStream::from_std(vsock_host.accept(apis::HOST_API_PORT).await?)?;
    let mut conn = rpc::GuestApiConnection::new(guest_api_stream);
    let host_api = rpc::serve_host_api(host_api_stream, HostApiServer);
    tokio::pin!(host_api);
    tokio::select! {
        r = session::run_session(&mut conn) => r,
        _ = &mut host_api => anyhow::bail!("host api server terminated"),
    }
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Old hello/shutdown demo flow
    Demo,
    /// Build a derivation from `nix derivation show -r` JSON
    Build {
        drv_json: PathBuf,
        #[arg(long, default_value = "https://cache.nixos.org")]
        cache: String,
        #[arg(long, default_value = ".cache")]
        cache_dir: PathBuf,
    },
    /// Run `nix --version` in the guest
    NixVersion,
    /// Evaluate a nix expression in the guest
    NixEval {
        /// Expression text, e.g. '1 + 1'
        #[arg(long)]
        expr: Option<String>,
        /// Read the expression from this file instead of --expr
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("error,alioth::board::aarch64=off"),
    )
    .init();
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Demo => {
            let hv = alioth::hv::Hvf {};
            let vm = alioth::vm::Machine::new(
                &hv,
                alioth::board::BoardConfig {
                    mem: alioth::mem::MemConfig {
                        size: 512 << 20,
                        backend: alioth::mem::MemBackend::Anonymous,
                        ..Default::default()
                    },
                    cpu: alioth::board::CpuConfig {
                        count: 1,
                        ..Default::default()
                    },
                    coco: None,
                },
            )?;
            vm.add_pl011()?;
            vm.add_pl031();
            vm.add_virtio_dev(
                "virtio-blk",
                alioth::virtio::dev::blk::BlkFileParam {
                    path: PathBuf::from("guest/nixdisk.erofs").into(),
                    readonly: true,
                    api: alioth::virtio::worker::WorkerApi::Mio,
                },
            )?;
            vm.add_virtio_dev(
                "virtio-rng",
                alioth::virtio::dev::entropy::EntropyParam::default(),
            )?;
            let (vsock_param, vsock_host) = VsockParam::new(3);
            vm.add_virtio_dev("virtio-vsock", vsock_param)?;

            vm.add_payload(alioth::loader::Payload {
                executable: Some(alioth::loader::Executable::Linux(
                    PathBuf::from("guest/vmlinux.bin").into(),
                )),
                cmdline: Some(std::ffi::CString::new(
                    "console=ttyAMA0 rdinit=/init quiet",
                )?),
                initramfs: Some(PathBuf::from("guest/initramfs.cpio.gz").into()),
                firmware: None,
            });

            vm.boot()?;

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_session(&vsock_host))?;

            vm.wait()?;
            Ok(())
        }
        Cmd::Build {
            drv_json,
            cache,
            cache_dir,
        } => orchestrator::run(orchestrator::BuildOpts {
            drv_json,
            cache_url: cache,
            cache_dir,
        })
        .map(|out_path| println!("built: {out_path}")),
        Cmd::NixVersion => nix_version(),
        Cmd::NixEval { expr, file } => {
            let expr = match (expr, file) {
                (Some(e), None) => e,
                (None, Some(f)) => std::fs::read_to_string(f)?,
                (None, None) => anyhow::bail!("pass exactly one of --expr or --file"),
                (Some(_), Some(_)) => anyhow::bail!("pass exactly one of --expr or --file"),
            };
            nix_eval(expr)
        }
    }
}

const NIX_CLOSURE_IMAGE: &str = "nix-closure.erofs";

fn boot_nix_vm() -> Result<(vm::Vm, String)> {
    let root = std::fs::read_to_string("guest/nix-closure/root")?
        .trim()
        .to_string();
    let vm = vm::Vm::boot(vm::VmSpec {
        mem_mib: 1024,
        cpus: 1,
        images: vec![image_fs::ImageFile {
            name: NIX_CLOSURE_IMAGE.to_string(),
            path: PathBuf::from(format!("guest/nix-closure/{NIX_CLOSURE_IMAGE}")),
        }],
        blk: vec![],
    })?;
    Ok((vm, root))
}

fn nix_version() -> Result<()> {
    let (vm, root) = boot_nix_vm()?;
    let version = vm.nix_version(NIX_CLOSURE_IMAGE.to_string(), root)?;
    orchestrator::reap_vm(vm, std::time::Duration::from_secs(60));
    println!("{version}");
    Ok(())
}

fn nix_eval(expr: String) -> Result<()> {
    let (vm, root) = boot_nix_vm()?;
    let result = vm.nix_eval(NIX_CLOSURE_IMAGE.to_string(), root, expr)?;
    orchestrator::reap_vm(vm, std::time::Duration::from_secs(60));
    println!("{result}");
    Ok(())
}
