//! Building one derivation inside a microVM.

use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};

use super::{Ctx, image_path};
use crate::image_fs::ImageFile;
use crate::vm::{BlkDev, Vm, VmSpec};

const OUTPUT_DEVICE_SIZE: u64 = 4 << 30;
const MAX_OUTPUT_DEVICES: usize = 29;

fn prep_output_device(path: &std::path::Path, output_name: &str) -> anyhow::Result<()> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    f.set_len(OUTPUT_DEVICE_SIZE)?;
    let mut label = vec![0u8; 64];
    let l = format!("{}{output_name}", apis::OUTPUT_LABEL_PREFIX);
    label[..l.len()].copy_from_slice(l.as_bytes());
    use std::io::{Seek, SeekFrom, Write};
    let mut f = f;
    f.seek(SeekFrom::Start(apis::OUTPUT_LABEL_OFFSET))?;
    f.write_all(&label)?;
    f.sync_all()?;
    Ok(())
}

pub(super) async fn build_one(ctx: &mut Ctx<'_>, drv_path: &str) -> anyhow::Result<()> {
    let drv = ctx.drvs[drv_path].clone();
    let outputs: Vec<_> = drv.outputs.iter().collect();
    println!(
        "build: {} ({} outputs, system {})",
        drv_path,
        outputs.len(),
        drv.system
    );

    let inputs = ctx.input_set(drv_path)?;
    println!("build: {} inputs", inputs.len());
    if outputs.len() > MAX_OUTPUT_DEVICES {
        bail!(
            "{drv_path} needs {} output devices, PCI topology supports at most {MAX_OUTPUT_DEVICES}",
            outputs.len()
        );
    }

    let mut images = Vec::with_capacity(inputs.len());
    let mut blk = Vec::with_capacity(outputs.len());
    let mut input_specs = Vec::with_capacity(inputs.len());
    for p in &inputs {
        let path = ctx
            .images
            .get(p)
            .cloned()
            .ok_or_else(|| anyhow!("missing image for input {p}"))?;
        images.push(ImageFile {
            name: apis::image_name(p),
            path,
        });
        input_specs.push(apis::InputSpec {
            store_path: p.clone(),
            volume_id: apis::volume_id(p),
        });
    }

    let mut out_devices: Vec<(String, String, PathBuf)> = Vec::new();
    for (name, out) in &outputs {
        let dev_path = ctx
            .opts
            .cache_dir
            .join("build")
            .join(format!("{}-{name}.img", &apis::store_hash(&out.path)[..16]));
        prep_output_device(&dev_path, name)
            .with_context(|| format!("preparing output device for {name}"))?;
        blk.push(BlkDev {
            path: dev_path.clone(),
            readonly: false,
        });
        out_devices.push((name.to_string(), out.path.clone(), dev_path));
    }

    let request = apis::BuildRequest {
        drv_path: drv_path.to_string(),
        builder: drv.builder.clone(),
        args: drv.args.clone(),
        env: drv
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        outputs: outputs
            .iter()
            .map(|(name, out)| apis::OutputSpec {
                name: name.to_string(),
                store_path: out.path.clone(),
            })
            .collect(),
        inputs: input_specs,
    };

    let vm = Vm::boot(VmSpec::guest(1024, 1, images, blk)).context("booting build vm")?;
    let result: apis::BuildResult = vm
        .guest_rpc(|c| async move { c.build(apis::tarpc::context::current(), request).await })
        .await
        .context("running build in vm")?;
    vm.reap(std::time::Duration::from_secs(60)).await;

    if !result.success {
        bail!(
            "build of {drv_path} failed: exit {:?} error {:?}",
            result.exit_code,
            result.error
        );
    }

    for img in &result.outputs {
        let (_, out_path, dev_path) = out_devices
            .iter()
            .find(|(n, _, _)| n == &img.name)
            .ok_or_else(|| anyhow!("guest reported unknown output {}", img.name))?;
        let f = std::fs::OpenOptions::new().write(true).open(dev_path)?;
        f.set_len(img.image_size)
            .context("truncating output image")?;
        let dest = image_path(ctx.opts, out_path);
        if dest.exists() {
            std::fs::remove_file(&dest)?;
        }
        std::fs::rename(dev_path, &dest)
            .with_context(|| format!("saving output image to {}", dest.display()))?;
        println!(
            "build: output {} -> {} ({} bytes)",
            out_path,
            dest.display(),
            img.image_size
        );
        ctx.images.insert(out_path.clone(), dest);
    }
    Ok(())
}
