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

use vm_controller_rpc::VmControllerClient;
use vm_controller_rpc::tarpc::client::NewClient;
use vm_controller_rpc::tarpc::context;
use vm_controller_rpc::tarpc::serde_transport::Transport;
use vm_controller_rpc::tarpc::tokio_serde::formats::Json;

mod vsock_device;
use vsock_device::{VsockHost, VsockParam};

const VSOCK_PORT: u32 = 5000;
const RPC_ATTEMPTS: usize = 50;

async fn run_session(vsock_host: &VsockHost) -> anyhow::Result<()> {
    for attempt in 1..=RPC_ATTEMPTS {
        let stream = match vsock_host.connect(VSOCK_PORT) {
            Ok(stream) => tokio::net::UnixStream::from_std(stream)?,
            Err(e) => {
                eprintln!("host: rpc attempt {attempt} failed: {e:#}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let transport = Transport::from((stream, Json::default()));
        let NewClient { client, dispatch } = VmControllerClient::new(
            vm_controller_rpc::tarpc::client::Config::default(),
            transport,
        );
        tokio::pin!(dispatch);

        let resp = tokio::select! {
            resp = client.hello(context::current(), "world".to_string()) => resp,
            _ = &mut dispatch => {
                eprintln!("host: rpc attempt {attempt} failed: dispatch terminated");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let resp = match resp {
            Ok(resp) => resp,
            Err(e) => {
                eprintln!("host: rpc attempt {attempt} failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        println!("host: guest replied: {resp}");

        let resp = tokio::select! {
            resp = client.shutdown(context::current()) => {
                resp.map_err(|e| anyhow::anyhow!("rpc call: {e}"))?
            }
            _ = &mut dispatch => anyhow::bail!("dispatch terminated"),
        };
        println!("host: guest replied: {resp}");
        return Ok(());
    }
    anyhow::bail!("rpc failed after {RPC_ATTEMPTS} attempts");
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
    rt.block_on(run_session(&vsock_host))?;

    vm.wait()?;
    Ok(())
}
