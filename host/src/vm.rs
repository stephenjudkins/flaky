use std::ffi::CString;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use alioth::board::{BoardConfig, CpuConfig};
use alioth::hv::Hvf;
use alioth::loader::{Executable, Payload};
use alioth::mem::{MemBackend, MemConfig};
use alioth::virtio::dev::blk::BlkFileParam;
use alioth::virtio::dev::entropy::EntropyParam;
use alioth::virtio::worker::WorkerApi;
use alioth::vm::Machine;
use anyhow::Context as _;

use crate::host_api::HostApiServer;
use crate::image_fs::{ImageFile, ImageFilesParam};
use crate::rpc::{self, GuestApiConnection};
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
}

pub struct Vm {
    machine: Machine<Hvf>,
    vsock_host: VsockHost,
}

const GUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

impl Vm {
    pub fn boot(spec: VmSpec) -> anyhow::Result<Vm> {
        let cmdline = String::from("console=ttyAMA0 rdinit=/init");
        let kernel = PathBuf::from("guest/vmlinux.bin");
        let hv = Hvf {};
        let vm = Machine::new(
            &hv,
            BoardConfig {
                mem: MemConfig {
                    size: spec.mem_mib << 20,
                    backend: MemBackend::Anonymous,
                    ..Default::default()
                },
                cpu: CpuConfig {
                    count: spec.cpus,
                    ..Default::default()
                },
                coco: None,
            },
        )?;

        vm.add_pl011()?;
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
                BlkFileParam {
                    path: dev.path.clone().into(),
                    readonly: dev.readonly,
                    api: WorkerApi::Mio,
                },
            )
            .with_context(|| format!("adding output block device {i}"))?;
        }
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

        Ok(Vm {
            machine: vm,
            vsock_host,
        })
    }

    /// Accepts the guest's vsock connections, serves the host API
    /// (logging), and performs one `build` RPC against the guest.
    pub fn run_build(&self, request: apis::BuildRequest) -> anyhow::Result<apis::BuildResult> {
        self.with_guest_api(|conn| {
            Box::pin(async move {
                conn.drive(|c| async move {
                    c.build(apis::tarpc::context::current(), request)
                        .await
                        .map_err(|e| anyhow::anyhow!("guest rpc: {e}"))
                })
                .await
            })
        })
    }

    /// Same as `run_build`, but performs a `nix_version` RPC: the guest
    /// mounts the named closure image, binds it into /nix/store, and runs
    /// `nix --version` from `nix_root`.
    pub fn nix_version(&self, image: String, nix_root: String) -> anyhow::Result<String> {
        self.with_guest_api(|conn| {
            Box::pin(async move {
                conn.drive(|c| async move {
                    c.nix_version(apis::tarpc::context::current(), image, nix_root)
                        .await
                        .map_err(|e| anyhow::anyhow!("guest rpc: {e}"))
                })
                .await
            })
        })
    }

    pub fn nix_eval(
        &self,
        image: String,
        nix_root: String,
        expr: String,
    ) -> anyhow::Result<String> {
        self.with_guest_api(|conn| {
            Box::pin(async move {
                conn.drive(|c| async move {
                    c.nix_eval(apis::tarpc::context::current(), image, nix_root, expr)
                        .await
                        .map_err(|e| anyhow::anyhow!("guest rpc: {e}"))
                })
                .await
            })
        })
    }

    fn with_guest_api<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: for<'a> FnOnce(
            &'a mut crate::rpc::GuestApiConnection,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<T>> + 'a>>,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let guest_api_stream = tokio::net::UnixStream::from_std(
                tokio::time::timeout(
                    GUEST_CONNECT_TIMEOUT,
                    self.vsock_host.accept(apis::GUEST_API_PORT),
                )
                .await
                .context("timed out waiting for guest api connection")??,
            )?;
            let host_api_stream = tokio::net::UnixStream::from_std(
                tokio::time::timeout(
                    GUEST_CONNECT_TIMEOUT,
                    self.vsock_host.accept(apis::HOST_API_PORT),
                )
                .await
                .context("timed out waiting for host api connection")??,
            )?;
            let mut conn = crate::rpc::GuestApiConnection::new(guest_api_stream);
            let host_api =
                crate::rpc::serve_host_api(host_api_stream, crate::host_api::HostApiServer);
            tokio::pin!(host_api);
            let result = tokio::select! {
                r = f(&mut conn) => r,
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
        })
    }

    pub fn wait(self) -> anyhow::Result<()> {
        self.machine.wait()?;
        Ok(())
    }
}
