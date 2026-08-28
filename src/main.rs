use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alioth::board::{BoardConfig, CpuConfig};
use alioth::hv::Hvf;
use alioth::loader::{Executable, Payload};
use alioth::mem::{MemBackend, MemConfig};
use alioth::virtio::dev::blk::BlkFileParam;
use alioth::virtio::dev::entropy::EntropyParam;
use alioth::virtio::dev::vsock::UdsVsockParam;
use alioth::virtio::worker::WorkerApi;
use alioth::vm::Machine;

use hello_rpc::HelloClient;
use hello_rpc::tarpc::context;
use hello_rpc::tarpc::serde_transport::Transport;
use hello_rpc::tarpc::tokio_serde::formats::Json;

const VSOCK_SOCK: &str = "guest/vsock.sock";
const VSOCK_PORT: u32 = 5000;

async fn call_hello() -> anyhow::Result<String> {
    let mut last_err = None;
    for _ in 0..50 {
        match try_call_hello().await {
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

async fn try_call_hello() -> anyhow::Result<String> {
    let mut stream = tokio::net::UnixStream::connect(VSOCK_SOCK).await?;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(format!("CONNECT {VSOCK_PORT}\n").as_bytes())
        .await?;
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("eof waiting for OK handshake");
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    anyhow::ensure!(
        line.starts_with(b"OK "),
        "unexpected vsock handshake: {:?}",
        String::from_utf8_lossy(&line)
    );
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
    let _ = std::fs::remove_file(Path::new(VSOCK_SOCK));
    vm.add_virtio_dev(
        "virtio-vsock",
        UdsVsockParam {
            cid: 3,
            path: PathBuf::from(VSOCK_SOCK).into_boxed_path(),
        },
    )?;

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
    let resp = rt.block_on(call_hello())?;
    println!("host: guest replied: {resp}");

    vm.wait()?;
    Ok(())
}
