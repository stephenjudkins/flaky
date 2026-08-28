use std::ffi::CString;
use std::path::PathBuf;
use std::time::Duration;

use alioth::board::{BoardConfig, CpuConfig};
use alioth::hv::Hvf;
use alioth::loader::{Executable, Payload};
use alioth::mem::{MemBackend, MemConfig};
use alioth::virtio::dev::blk::BlkFileParam;
use alioth::virtio::dev::entropy::EntropyParam;
use alioth::virtio::worker::WorkerApi;
use alioth::vm::Machine;

use hello_rpc::HelloClient;
use hello_rpc::tarpc::context;
use hello_rpc::tarpc::serde_transport::Transport;
use hello_rpc::tarpc::tokio_serde::formats::Json;

mod vsock_device;
use vsock_device::{VsockHost, VsockParam};

const VSOCK_PORT: u32 = 5000;

async fn call_hello(vsock_host: &VsockHost) -> anyhow::Result<String> {
    let mut last_err = None;
    for _ in 0..50 {
        match try_call_hello(vsock_host).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                eprintln!("host: rpc attempt failed: {e:#}");
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Err(anyhow::anyhow!(
        "rpc failed after retries: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

async fn try_call_hello(vsock_host: &VsockHost) -> anyhow::Result<String> {
    let stream = vsock_host.connect(VSOCK_PORT)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let transport = Transport::from((stream, Json::default()));
    let client = HelloClient::new(hello_rpc::tarpc::client::Config::default(), transport).spawn();
    let resp = client
        .hello(context::current(), "world".to_string())
        .await
        .map_err(|e| anyhow::anyhow!("rpc call: {e}"))?;
    Ok(resp)
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cmdline = String::from("console=ttyAMA0 rdinit=/init");

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
    let (vsock_param, vsock_host) = VsockParam::new(3)?;
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
    let resp = rt.block_on(call_hello(&vsock_host))?;
    println!("host: guest replied: {resp}");

    vm.wait()?;
    Ok(())
}
