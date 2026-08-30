use std::ffi::CString;
use std::path::PathBuf;

use alioth::board::{BoardConfig, CpuConfig};
use alioth::hv::Hvf;
use alioth::loader::{Executable, Payload};
use alioth::mem::{MemBackend, MemConfig};
use alioth::virtio::dev::blk::BlkFileParam;
use alioth::virtio::dev::entropy::EntropyParam;
use alioth::virtio::worker::WorkerApi;
use alioth::vm::Machine;

use apis::{GUEST_API_PORT, HOST_API_PORT};

mod host_api;
mod rpc;
mod session;
mod vsock_device;
use host_api::HostApiServer;
use vsock_device::{VsockHost, VsockParam};

async fn run_session(vsock_host: &VsockHost) -> anyhow::Result<()> {
    let guest_api_stream =
        tokio::net::UnixStream::from_std(vsock_host.accept(GUEST_API_PORT).await?)?;
    let host_api_stream =
        tokio::net::UnixStream::from_std(vsock_host.accept(HOST_API_PORT).await?)?;
    let mut conn = rpc::GuestApiConnection::new(guest_api_stream);
    let host_api = rpc::serve_host_api(host_api_stream, HostApiServer);
    tokio::pin!(host_api);
    tokio::select! {
        r = session::run_session(&mut conn) => r,
        _ = &mut host_api => anyhow::bail!("host api server terminated"),
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("error,alioth::board::aarch64=off"),
    )
    .init();
    let cmdline = String::from("console=ttyAMA0 rdinit=/init quiet");

    let kernel = PathBuf::from("guest/vmlinux.bin");
    let hv = Hvf {};
    let vm = Machine::new(
        &hv,
        BoardConfig {
            mem: MemConfig {
                size: 512 << 20,
                backend: MemBackend::Anonymous,
                ..Default::default()
            },
            cpu: CpuConfig {
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
        BlkFileParam {
            path: PathBuf::from("guest/nixdisk.erofs").into(),
            readonly: true,
            api: WorkerApi::Mio,
        },
    )?;
    vm.add_virtio_dev("virtio-rng", EntropyParam::default())?;
    let (vsock_param, vsock_host) = VsockParam::new(3);
    vm.add_virtio_dev("virtio-vsock", vsock_param)?;

    vm.add_payload(Payload {
        executable: Some(Executable::Linux(kernel.into())),
        cmdline: Some(CString::new(cmdline).unwrap()),
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
