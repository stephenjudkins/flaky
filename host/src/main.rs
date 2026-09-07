use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

mod fetchurl;
mod flake;
mod host_api;
mod image_fs;
mod nar;
mod orchestrator;
mod rpc;
mod tarball;
mod vm;
mod vsock_device;
mod vsock_pipe;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a derivation from `nix derivation show -r` JSON
    Build {
        drv_json: PathBuf,
        #[arg(long, default_value = "https://cache.nixos.org")]
        cache: String,
        #[arg(long, default_value = ".cache")]
        cache_dir: PathBuf,
    },
    /// Build a flake attribute, evaluating it with nix in a guest VM
    Flake {
        flake_ref: String,
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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main(cli.cmd))
}

async fn async_main(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Build {
            drv_json,
            cache,
            cache_dir,
        } => {
            let text = std::fs::read_to_string(&drv_json)
                .with_context(|| format!("reading {}", drv_json.display()))?;
            orchestrator::run(orchestrator::BuildOpts {
                drv_json: text,
                cache_url: cache,
                cache_dir,
                extra_images: Default::default(),
            })
            .await
            .map(|out_path| println!("built: {out_path}"))
        }
        Cmd::Flake {
            flake_ref,
            cache,
            cache_dir,
        } => {
            let r = flake::parse_flake_ref(&flake_ref)?;
            flake::run(r, cache, cache_dir)
                .await
                .map(|out_path| println!("built: {out_path}"))
        }
        Cmd::NixVersion => nix_version().await,
        Cmd::NixEval { expr, file } => {
            let expr = match (expr, file) {
                (Some(e), None) => e,
                (None, Some(f)) => std::fs::read_to_string(f)?,
                (None, None) => anyhow::bail!("pass exactly one of --expr or --file"),
                (Some(_), Some(_)) => anyhow::bail!("pass exactly one of --expr or --file"),
            };
            nix_eval(expr).await
        }
    }
}

const NIX_CLOSURE_IMAGE: &str = "nix-closure.erofs";

async fn boot_nix_vm() -> Result<(vm::Vm, String)> {
    let root = std::fs::read_to_string("guest/nix-closure/root")?
        .trim()
        .to_string();
    let vm = vm::Vm::boot(vm::VmSpec::guest(
        1024,
        1,
        vec![image_fs::ImageFile {
            name: NIX_CLOSURE_IMAGE.to_string(),
            path: PathBuf::from(format!("guest/nix-closure/{NIX_CLOSURE_IMAGE}")),
        }],
        vec![],
    ))?;
    Ok((vm, root))
}

async fn nix_version() -> Result<()> {
    let (vm, root) = boot_nix_vm().await?;
    let version = vm
        .guest_rpc(|c| async move {
            c.nix_version(
                crate::rpc::rpc_context(),
                NIX_CLOSURE_IMAGE.to_string(),
                root,
            )
            .await
            .map_err(|e| anyhow::anyhow!("guest rpc: {e}"))?
            .map_err(anyhow::Error::msg)
        })
        .await?;
    vm.reap(std::time::Duration::from_secs(60)).await;
    println!("{version}");
    Ok(())
}

async fn nix_eval(expr: String) -> Result<()> {
    let (vm, root) = boot_nix_vm().await?;
    let result = vm
        .guest_rpc(|c| async move {
            c.nix_eval(
                crate::rpc::rpc_context(),
                NIX_CLOSURE_IMAGE.to_string(),
                root,
                expr,
            )
            .await
            .map_err(|e| anyhow::anyhow!("guest rpc: {e}"))?
            .map_err(anyhow::Error::msg)
        })
        .await?;
    vm.reap(std::time::Duration::from_secs(60)).await;
    println!("{result}");
    Ok(())
}
