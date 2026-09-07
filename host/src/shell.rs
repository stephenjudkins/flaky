//! `flaky shell nixpkgs#<attr>...`: evaluate and realize packages from a
//! pinned nixpkgs in microvms, then boot a shell microvm with the packages'
//! runtime reference closures mounted under /nix/store. Mirrors
//! `nix shell`: only PATH (plus HOME/TERM) is set for the user's shell.
//! Evaluation goes through the flake machinery: a tiny synthetic flake
//! pins nixpkgs and passes its legacyPackages through, so eval caching and
//! input fetching all behave like `flaky flake`. Realization is delegated
//! to the orchestrator.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};

use crate::flake;
use crate::image_fs::ImageFile;

const NIXPKGS_REV: &str = "2c423e03bbafcff28bfadc6781a4a8257f205cb5";
const NIXPKGS_NAR_HASH: &str = "sha256-dt4WdcvsA8/RCe+VZZwqU0X+XMM3wBbGCWA0/sFWzGo=";
const NIXPKGS_LAST_MODIFIED: i64 = 1787360063;
const ATTR_PREFIX: &str = "legacyPackages.aarch64-linux";
const SHELL_ATTR: &str = "bash";

pub async fn run(
    installables: Vec<String>,
    cache_url: String,
    cache_dir: PathBuf,
) -> anyhow::Result<()> {
    let mut attrs: Vec<String> = Vec::new();
    for inst in &installables {
        let attr = inst
            .strip_prefix("nixpkgs#")
            .filter(|a| !a.is_empty() && !a.contains('#'))
            .with_context(|| format!("unsupported installable {inst} (want nixpkgs#<attr>)"))?;
        let full = format!("{ATTR_PREFIX}.{attr}");
        if !attrs.contains(&full) {
            attrs.push(full);
        }
    }
    let bash_attr = format!("{ATTR_PREFIX}.{SHELL_ATTR}");
    if !attrs.contains(&bash_attr) {
        attrs.push(bash_attr.clone());
    }

    let dir = synth_flake(&cache_dir)?;
    let evaluated = flake::eval(&dir, &attrs, &cache_dir).await?;

    let mut out_paths: BTreeMap<String, String> = BTreeMap::new();
    let mut closure_paths = std::collections::BTreeSet::new();
    for attr in &attrs {
        let realized = crate::orchestrator::realize_closure(crate::orchestrator::BuildOpts {
            drv_json: evaluated.drv_jsons[attr].clone(),
            cache_url: cache_url.clone(),
            cache_dir: cache_dir.clone(),
            extra_images: evaluated.extra_images.clone(),
        })
        .await
        .with_context(|| format!("realizing {attr}"))?;
        out_paths.insert(attr.clone(), realized.out_path);
        closure_paths.extend(realized.paths);
    }

    let images: Vec<ImageFile> = closure_paths
        .iter()
        .map(|p| ImageFile {
            name: apis::image_name(p),
            path: crate::orchestrator::image_path_for(&cache_dir, p),
        })
        .collect();
    let inputs: Vec<apis::InputSpec> = closure_paths
        .iter()
        .map(|p| apis::InputSpec {
            store_path: p.clone(),
            volume_id: apis::volume_id(p),
        })
        .collect();
    let shell_bin = format!("{}/bin/bash", out_paths[&bash_attr]);
    let path_entries: Vec<String> = attrs
        .iter()
        .filter(|a| **a != bash_attr)
        .chain(std::iter::once(&bash_attr))
        .filter_map(|a| out_paths.get(a).map(|out| format!("{out}/bin")))
        .collect();
    let term = std::env::var("TERM").unwrap_or_else(|_| "linux".to_string());

    println!(
        "shell: {} store paths, {} images; booting shell vm",
        closure_paths.len(),
        images.len()
    );
    let spec = crate::vm::VmSpec::guest(4096, 2, images, vec![]);
    let vm = crate::vm::Vm::boot(spec).context("booting shell vm")?;
    let req = apis::ShellRequest {
        inputs,
        shell: shell_bin,
        path: path_entries,
        term,
    };
    let code = vm
        .guest_rpc(|c| async move {
            c.shell(crate::rpc::rpc_context(), req)
                .await
                .map_err(|e| anyhow!("shell rpc: {e}"))?
                .map_err(anyhow::Error::msg)
        })
        .await?;
    vm.reap(std::time::Duration::from_secs(60)).await;
    std::process::exit(code);
}

/// Writes the synthetic flake under the cache dir and returns its dir: a
/// pinned-nixpkgs input plus an attr-independent legacyPackages passthrough,
/// so `nixpkgs#<attr>` becomes `<dir>#legacyPackages.aarch64-linux.<attr>`
/// and the stable contents keep eval cache keys stable.
fn synth_flake(cache_dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = cache_dir.join("tmp").join("shell-flake");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("flake.nix"),
        format!(
            "{{\n  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/{NIXPKGS_REV}\";\n  outputs = {{ self, nixpkgs }}: {{\n    legacyPackages.aarch64-linux = nixpkgs.legacyPackages.aarch64-linux;\n  }};\n}}\n"
        ),
    )?;
    let lock = serde_json::json!({
        "nodes": {
            "nixpkgs": {
                "locked": {
                    "lastModified": NIXPKGS_LAST_MODIFIED,
                    "narHash": NIXPKGS_NAR_HASH,
                    "owner": "NixOS",
                    "repo": "nixpkgs",
                    "rev": NIXPKGS_REV,
                    "type": "github"
                },
                "original": {
                    "owner": "NixOS",
                    "repo": "nixpkgs",
                    "rev": NIXPKGS_REV,
                    "type": "github"
                }
            },
            "root": {
                "inputs": { "nixpkgs": "nixpkgs" }
            }
        },
        "root": "root",
        "version": 7
    });
    std::fs::write(dir.join("flake.lock"), serde_json::to_string_pretty(&lock)?)?;
    Ok(dir)
}
