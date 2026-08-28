# Rust Crates for Library-Level VM Support (Linux MicroVMs)

*Researched 2026-08-22. Target: run Linux microvms from Rust, on both Linux and macOS hosts; possible custom virtio-fs device.*

## TL;DR

| Host | Recommended path | Custom virtio-fs? |
|---|---|---|
| Linux | `kvm-ioctls` + rust-vmm crates (DIY), or Cloud Hypervisor / **alioth** as libraries | ✅ Feasible and well-trodden |
| macOS | **alioth** (HVF) as a library, or `arcbox-vz` (Virtualization.framework) for least code | ⚠️ Only via HVF-based custom VMM (e.g. alioth); Virtualization.framework allows only Apple's fixed devices |

There is no mature cross-platform hypervisor abstraction crate in Rust today; expect different code paths per host.

---

## Linux hosts (KVM)

### Low-level building blocks (rust-vmm)

The [rust-vmm](https://github.com/rust-vmm) project publishes a mature, actively maintained set of library crates:

| Crate | Role | Status |
|---|---|---|
| [`kvm-ioctls`](https://github.com/rust-vmm/kvm) | Safe KVM ioctl wrappers (`Kvm`/`VmFd`/`VcpuFd`) | Active (v0.25, Jun 2026), Apache-2.0/MIT |
| [`kvm-bindings`](https://crates.io/crates/kvm-bindings) | FFI structs/constants for KVM | Active |
| [`vm-memory`](https://crates.io/crates/vm-memory) | Guest memory abstraction (`GuestMemoryMmap`, dirty tracking) | Production-ready |
| [`linux-loader`](https://github.com/rust-vmm/linux-loader) | ELF/bzImage/PE kernel loading + cmdline | Active (v0.14, Jun 2026) |
| [`virtio-queue`](https://docs.rs/virtio-queue) / vm-virtio workspace | Virtqueue + device plumbing for writing your own virtio devices | Active, fuzzed |
| `vmm-sys-util`, `vm-device`, `vm-allocator`, `event-manager`, `seccompiler` | Syscall helpers, device traits, IRQ allocation | Production-ready tier |

This stack gives you everything except the vCPU setup loop, MMIO bus wiring, and device models — you write those. The best blueprint is the rust-vmm [vmm-reference](https://github.com/rust-vmm/vmm-reference), a working micro-VMM composed entirely from these crates.

⚠️ **Name trap:** the crates.io [`hypervisor`](https://crates.io/crates/hypervisor) crate is an abandoned 2017 macOS toy, unrelated to KVM. The rust-vmm `hypervisor` abstraction lives inside the Cloud Hypervisor repo and is not published to crates.io.

### Full VMMs usable as libraries

- **[Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor)** (Apache-2.0, very active): a Cargo workspace of library crates (`hypervisor`, `devices`, `virtio-devices`, `vmm`, …) behind a thin binary. Dependable via git path deps to compose your own VMM. Caveat: internal APIs are unstable between releases.
- **[Firecracker](https://github.com/firecracker-microvm/firecracker)** (Apache-2.0, very active): effectively binary-only. One microVM per process, controlled over a Unix-socket API; internal crates are unpublished and embedding is explicitly unsupported. Use it by spawning the process, not as a library.
- **[crosvm](https://github.com/google/crosvm)** (Google/ChromeOS): large binary with gn/Bazel build, heavy ChromeOS coupling. Poor library usability.
- **[libkrun](https://github.com/containers/libkrun)** (MIT/Apache-2.0, active; Rust wrapper crate `libkrun` v1.19): a shared library for running container-like Linux workloads in a lightweight VM. Great for process isolation, poor if you want fine-grained VMM control.
- **[google/alioth](https://github.com/google/alioth)** (Apache-2.0, active): experimental Type-2 hypervisor using **KVM on Linux and HVF on macOS**. Ships virtio-blk/net/vsock/**fs**/entropy/balloon, boots real Linux kernels, published as crates. The only notable cross-platform (Linux+macOS) Rust VMM-as-library.

---

## macOS hosts

Two distinct Apple APIs, very different trade-offs:

### Virtualization.framework (VZF) — high level, fixed device set

Apple handles firmware, GIC, and virtio emulation; you configure a fixed catalog of devices (block, network, socket, entropy, balloon, **and `VZVirtioFileSystemDeviceConfiguration` on macOS 12+**). You cannot inject arbitrary custom virtio devices.

Rust bindings:

| Crate | Status | Notes |
|---|---|---|
| [`arcbox-vz`](https://github.com/arcboxlabs/arcbox) | Active (v0.7) | Best current maintained VZF binding |
| `vz` (crates.io) | ⚠️ squatted | Now an unrelated crate; historical binding was GitHub-only |
| `vfkit` | Not Rust | Go CLI from crc-org |
| `vm-rs` | Experimental (v0.2) | Cross-platform VZF + Cloud Hypervisor lifecycle wrapper, very early |

Linux guests on Apple Silicon work well under VZF (see Tart, UTM).

### Hypervisor.framework (HVF) — low level, build everything

Raw VM/vCPU/memory primitives only; no device models. Bindings:

| Crate | Status | Notes |
|---|---|---|
| [`applevisor`](https://github.com/Impalabs/applevisor) | Maintained (v1.0, Jan 2026) | AArch64-only, Apache-2.0, cleanest modern option |
| [`hv`/`hv-sys`](https://github.com/cloud-hypervisor/hypervisor-framework) | Stale (2022) | x86_64 + aarch64, but unmaintained |
| [`xhypervisor`](https://github.com/RWTH-OS/xhypervisor) | Semi-active | Unsafe bindings, minimal docs |
| `hypervisor` (crates.io) | Dead (2017) | Intel-only era; avoid |

With HVF you must write the entire VMM yourself: virtio bus, serial, GIC interrupt controller, firmware/kernel loading. **[alioth](https://github.com/google/alioth) has already done this** and is the pragmatic starting point.

### Constraints

- No nested virtualization on either HVF or VZF (no KVM-in-VM).
- Binaries need the `com.apple.security.hypervisor` entitlement (self-signing suffices for dev).
- No viable native KVM-on-macOS; the rust-vmm `hypervisor` crate has KVM/MSHV/WHPX backends but **no HVF backend**.
- Ecosystem has moved to Apple Silicon; Intel support is fading.

---

## Custom virtio-fs: feasibility

**Spec:** virtio-fs is standardized (OASIS virtio 1.3, Device ID 26). Config space is a `tag` + queue count; the payload protocol is the **FUSE ABI** over virtqueues, plus an optional DAX window. The real implementation cost is FUSE request dispatch, not the virtio plumbing.

**Guest side:** Linux ships the `virtiofs` kernel driver out of the box — guests need no custom driver. There is essentially no mature userspace/Rust guest driver for virtio-fs, so plan on Linux guests.

**Host side — what exists:**
- QEMU's `virtiofsd` was rewritten in Rust atop rust-vmm crates ([source](https://gitlab.com/qemu-project/virtiofsd)) — proof a full Rust implementation exists and a rich source to lift logic from.
- Cloud Hypervisor historically shipped an in-process virtio-fs device plus vhost-user-fs (recently reorganized around vhost-user-fs).
- crosvm and libkrun both support virtio-fs.
- **Firecracker does not support virtio-fs and explicitly rejects feature requests for it** — you'd have to fork it.

**Writing your own custom device:**
- **On KVM/Linux: feasible and well-trodden.** Compose `virtio-queue` (ring/descriptor handling) + `vm-memory` (guest memory access) + transport emulation (PCI/MMIO), then implement your own FUSE backend (backed by a real filesystem or something synthetic). This mirrors how Firecracker implements its in-process block/net devices. No published crate provides the virtio-fs/FUSE layer itself — that part is yours (or vendored from virtiofsd).
- **On macOS:** not possible under Virtualization.framework's fixed device catalog *unless* you use Apple's built-in `VZVirtioFileSystemDeviceConfiguration` (which is not customizable beyond its directory sharing semantics). A truly custom virtio-fs device requires a hand-built HVF VMM — i.e., base your macOS path on alioth, which already includes a virtio-fs device you can fork/extend.

One spec caveat: virtio-fs §5.11.6.5 has the device trusting driver-supplied uid/gid — acceptable for single-tenant microVMs, worth knowing for multi-tenant designs.

---

## Recommendation

1. **Shared architecture, per-host backends.** Design your runner around a small trait boundary (vCPU run, memory mapping, IRQ injection); implement it with `kvm-ioctls` on Linux and HVF bindings (`applevisor`) or alioth's hypervisor module on macOS.
2. **Fastest to working microvms:** adopt [alioth](https://github.com/google/alioth) as a library/fork — it already spans KVM + HVF, boots Linux, and ships virtio-fs on both platforms. Experimental, but nothing else covers both hosts.
3. **Maximum control on Linux:** rust-vmm crates + `vmm-reference` as blueprint.
4. **Least code on macOS (if custom virtio-fs isn't required):** `arcbox-vz`.
5. **For virtio-fs customization:** start from alioth's or virtiofsd's Rust implementation rather than from scratch; keep guests on stock Linux kernels so the built-in `virtiofs` driver works unchanged.
