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

use apis::GuestApiClient;
use apis::HostApi;
use apis::Postcard;
use apis::tarpc::client::NewClient;
use apis::tarpc::context;
use apis::tarpc::serde_transport::Transport;
use apis::tarpc::server::BaseChannel;
use apis::tarpc::server::Channel;
use apis::{GUEST_API_PORT, HOST_API_PORT};
use futures::prelude::*;

mod vsock_device;
use vsock_device::{VsockHost, VsockParam};

#[derive(Clone)]
struct HostApiServer;

impl HostApi for HostApiServer {
    async fn greet(self, _: context::Context, x: String) -> String {
        format!("hello from host: {x}")
    }
}

async fn run_session(vsock_host: &VsockHost) -> anyhow::Result<()> {
    let guest_api_stream =
        tokio::net::UnixStream::from_std(vsock_host.accept(GUEST_API_PORT).await?)?;
    let transport = Transport::from((guest_api_stream, Postcard::default()));
    let NewClient { client, dispatch } =
        GuestApiClient::new(apis::tarpc::client::Config::default(), transport);
    tokio::pin!(dispatch);

    let host_api_stream =
        tokio::net::UnixStream::from_std(vsock_host.accept(HOST_API_PORT).await?)?;
    let host_transport = Transport::from((host_api_stream, Postcard::default()));
    let channel = BaseChannel::with_defaults(host_transport);
    let host_server = channel
        .execute(HostApiServer.serve())
        .for_each(|resp| async {
            let _ = resp.await;
        });
    tokio::pin!(host_server);

    let resp = tokio::select! {
        resp = client.hello(context::current(), "world".to_string()) => {
            resp.map_err(|e| anyhow::anyhow!("rpc call: {e}"))?
        }
        _ = &mut dispatch => anyhow::bail!("dispatch terminated"),
        _ = &mut host_server => anyhow::bail!("host api server terminated"),
    };
    println!("host: guest replied: {resp}");

    let resp = tokio::select! {
        resp = client.shutdown(context::current()) => {
            resp.map_err(|e| anyhow::anyhow!("rpc call: {e}"))?
        }
        _ = &mut dispatch => anyhow::bail!("dispatch terminated"),
        _ = &mut host_server => anyhow::bail!("host api server terminated"),
    };
    println!("host: guest replied: {resp}");
    Ok(())
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
