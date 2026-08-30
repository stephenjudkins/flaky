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
    }
}
