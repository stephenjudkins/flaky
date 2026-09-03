use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use alioth::board::{BoardSpec, CpuSpec, CpuTopology};
use alioth::fuse::passthrough::Passthrough;
use alioth::hv::Hvf;
use alioth::loader::{Executable, PayloadSpec};
use alioth::mem::{MemBackend, MemSpec};
use alioth::virtio::dev::DevSpec;
use alioth::virtio::dev::blk::BlkFileSpec;
use alioth::virtio::dev::entropy::EntropySpec;
use alioth::virtio::dev::fs::{Fs, FsConfig};
use alioth::virtio::worker::WorkerApi;
use alioth::vm::Machine;
use anyhow::Context as _;

use crate::image_fs::{ImageFile, ImageFilesParam};
use crate::vsock_device::{VsockHost, VsockParam};

pub struct BlkDev {
    pub path: PathBuf,
    pub readonly: bool,
}

pub struct VmSpec {
    pub mem_mib: u64,
    pub cpus: u16,
    pub images: Vec<ImageFile>,
    pub blk: Vec<BlkDev>,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub cmdline: String,
    /// Host directory exposed read-only via a `flaky-src` virtio-fs device.
    pub src_dir: Option<PathBuf>,
}

impl VmSpec {
    pub fn guest(mem_mib: u64, cpus: u16, images: Vec<ImageFile>, blk: Vec<BlkDev>) -> Self {
        VmSpec {
            mem_mib,
            cpus,
            images,
            blk,
            kernel: PathBuf::from("guest/vmlinux.bin"),
            initramfs: PathBuf::from("guest/initramfs.cpio.gz"),
            cmdline: "console=ttyAMA0 quiet rdinit=/init".to_string(),
            src_dir: None,
        }
    }
}

const SRC_TAG: &str = "flaky-src";

struct DirFsParam {
    path: PathBuf,
}

impl DevSpec for DirFsParam {
    type Device = Fs<Passthrough>;

    fn build(
        self,
        name: impl Into<std::sync::Arc<str>>,
    ) -> Result<Self::Device, alioth::virtio::Error> {
        let filesystem = Passthrough::new(self.path.into_boxed_path())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut config = FsConfig {
            tag: [0; 36],
            num_request_queues: 1,
            notify_buf_size: 0,
        };
        config.tag[..SRC_TAG.len()].copy_from_slice(SRC_TAG.as_bytes());
        Fs::new(name, filesystem, config, 0)
    }
}

pub struct Vm {
    machine: Machine<Hvf>,
    vsock_host: VsockHost,
}

const GUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

impl Vm {
    pub fn boot(spec: VmSpec) -> anyhow::Result<Vm> {
        let hv = Hvf {};
        let vm = Machine::new(
            &hv,
            BoardSpec {
                mem: MemSpec {
                    size: spec.mem_mib << 20,
                    backend: MemBackend::Anonymous,
                    ..Default::default()
                },
                cpu: CpuSpec {
                    count: spec.cpus,
                    topology: CpuTopology {
                        sockets: 1,
                        cores: spec.cpus,
                        ..Default::default()
                    },
                },
                coco: None,
            },
        )?;

        vm.add_pl011(&alioth::device::console::ConsoleSpec::Stdio)?;
        vm.add_pl031();

        vm.add_virtio_dev(
            "virtio-fs-images",
            ImageFilesParam {
                tag: "flaky-images".to_string(),
                files: spec.images,
            },
        )?;
        for (i, dev) in spec.blk.iter().enumerate() {
            vm.add_virtio_dev(
                format!("virtio-blk-{i}"),
                BlkFileSpec {
                    path: dev.path.clone().into(),
                    readonly: dev.readonly,
                    api: WorkerApi::Mio,
                },
            )
            .with_context(|| format!("adding output block device {i}"))?;
        }
        if let Some(dir) = &spec.src_dir {
            vm.add_virtio_dev("virtio-fs-src", DirFsParam { path: dir.clone() })?;
        }
        vm.add_virtio_dev("virtio-rng", EntropySpec::default())?;
        let (vsock_param, vsock_host) = VsockParam::new(3);
        vm.add_virtio_dev("virtio-vsock", vsock_param)?;

        vm.add_payload(PayloadSpec {
            executable: Some(Executable::Linux(spec.kernel.into())),
            cmdline: Some(spec.cmdline.into_boxed_str()),
            initramfs: Some(spec.initramfs.into()),
            firmware: None,
        });

        vm.boot()?;

        Ok(Vm {
            machine: vm,
            vsock_host,
        })
    }

    /// Accepts the guest's vsock connections, serves the host API
    /// (logging), and performs RPCs against the guest. `f` receives the
    /// tarpc client and returns the in-flight request future; it may issue
    /// multiple sequential RPCs.
    pub async fn guest_rpc<T, F, Fut>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(crate::rpc::GuestApiClient) -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let guest_api_stream = tokio::time::timeout(
            GUEST_CONNECT_TIMEOUT,
            self.vsock_host.accept(apis::GUEST_API_PORT),
        )
        .await
        .context("timed out waiting for guest api connection")??;
        let host_api_stream = tokio::time::timeout(
            GUEST_CONNECT_TIMEOUT,
            self.vsock_host.accept(apis::HOST_API_PORT),
        )
        .await
        .context("timed out waiting for host api connection")??;
        let mut conn = crate::rpc::GuestApiConnection::new(guest_api_stream);
        let host_api = crate::rpc::serve_host_api(host_api_stream, crate::host_api::HostApiServer);
        tokio::pin!(host_api);
        let result = tokio::select! {
            r = conn.drive(f) => r,
            _ = &mut host_api => anyhow::bail!("host api server terminated"),
        };
        // always ask the guest to power off so vm.wait() completes
        let _ = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            conn.drive(|c| async move {
                c.shutdown(apis::tarpc::context::current())
                    .await
                    .map_err(|e| anyhow::anyhow!("guest rpc: {e}"))
            }),
        )
        .await;
        result
    }

    pub fn wait(self) -> anyhow::Result<()> {
        self.machine.wait()?;
        Ok(())
    }

    /// Waits for the VM to power off, warning (not failing) on timeout or
    /// error. Callers use this after `guest_rpc`, which already asked the
    /// guest to shut down.
    pub async fn reap(self, timeout: Duration) {
        let result =
            tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || self.wait())).await;
        match result {
            Ok(Ok(r)) => {
                if let Err(e) = r {
                    eprintln!("warning: vm exited with error: {e:#}");
                }
            }
            Ok(Err(e)) => eprintln!("warning: vm wait task failed: {e:#}"),
            Err(_) => eprintln!(
                "warning: vm did not power off within {timeout:?}; continuing with vm still running"
            ),
        }
    }
}
