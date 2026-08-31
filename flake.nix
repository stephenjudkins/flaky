{
  description = "A simple Rust development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      forLinuxSystems = f: nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (
        pkgs:
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
            ];
            shellHook = ''
              unset SDKROOT DEVELOPER_DIR
            '';
          };
        }
      );
      packages = forLinuxSystems (
        pkgs:
        let
          bp = pkgs.buildPackages;
          src = nixpkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              nixpkgs.lib.hasPrefix "${toString ./guest-runtime}" path
              || nixpkgs.lib.hasPrefix "${toString ./apis}" path
              || nixpkgs.lib.hasPrefix "${toString ./host}" path
              || nixpkgs.lib.hasPrefix "${toString ./nix-drv}" path
              || nixpkgs.lib.hasPrefix "${toString ./nar-to-erofs}" path
              || nixpkgs.lib.hasPrefix "${toString ./nix-cache}" path
              || builtins.elem path [
                (toString ./Cargo.toml)
                (toString ./Cargo.lock)
              ];
          };
          guest-runtime = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
            pname = "guest-runtime";
            version = "0.1.0";
            inherit src;
            buildAndTestSubdir = "guest-runtime";
            cargoLock.lockFile = ./Cargo.lock;
          };
          guest-kernel-base = pkgs.linux_latest.override {
            enableCommonConfig = false;
            autoModules = false;
            structuredExtraConfig = with pkgs.lib.kernel; {
              MODULES = no;
              DEBUG_INFO_NONE = yes;
              # boot-time optimization: disable every subsystem this appliance
              # cannot use (no display/input/usb/nic/real storage, DT boot)
              EXPERT = yes;
              # all 53 arm64 platform SoC families off; QEMU virt uses only
              # generic DT drivers (pl011, pl031, GICv3, arch timer, ECAM)
              ARCH_ACTIONS = no;
              ARCH_AIROHA = no;
              ARCH_SUNXI = no;
              ARCH_ALPINE = no;
              ARCH_APPLE = no;
              ARCH_ASPEED = no;
              ARCH_AXIADO = no;
              ARCH_BCM2835 = no;
              ARCH_BCM_IPROC = no;
              ARCH_BCMBCA = no;
              ARCH_BRCMSTB = no;
              ARCH_BERLIN = no;
              ARCH_BITMAIN = no;
              ARCH_BLAIZE = no;
              ARCH_BST = no;
              ARCH_CIX = no;
              ARCH_EXYNOS = no;
              ARCH_K3 = no;
              ARCH_LG1K = no;
              ARCH_HISI = no;
              ARCH_KEEMBAY = no;
              ARCH_MEDIATEK = no;
              ARCH_MESON = no;
              ARCH_LAN969X = no;
              ARCH_SPARX5 = no;
              ARCH_MMP = no;
              ARCH_MVEBU = no;
              ARCH_LAYERSCAPE = no;
              ARCH_MXC = no;
              ARCH_S32 = no;
              ARCH_MA35 = no;
              ARCH_NPCM = no;
              ARCH_PENSANDO = no;
              ARCH_QCOM = no;
              ARCH_REALTEK = no;
              ARCH_RENESAS = no;
              ARCH_ROCKCHIP = no;
              ARCH_SEATTLE = no;
              ARCH_INTEL_SOCFPGA = no;
              ARCH_SOPHGO = no;
              ARCH_STM32 = no;
              ARCH_SYNQUACER = no;
              ARCH_TEGRA = no;
              ARCH_SPRD = no;
              ARCH_THUNDER = no;
              ARCH_THUNDER2 = no;
              ARCH_UNIPHIER = no;
              ARCH_VEXPRESS = no;
              ARCH_VISCONTI = no;
              ARCH_XGENE = no;
              ARCH_ZYNQMP = no;
              PINCTRL = no;
              GPIOLIB = no;
              MAILBOX = no;
              EXTCON = no;
              REGULATOR = no;
              MMC = no;
              MD = no;
              NEW_LEDS = no;
              MEDIA_SUPPORT = no;
              SOUND = no;
              TCG_TPM = no;
              IPMI_HANDLER = no;
              EDAC = no;
              PM_DEVFREQ = no;
              RFKILL = no;
              TUN = no;
              VFIO = no;
              MTD = no;
              QUOTA = no;
              NETWORK_FILESYSTEMS = no;
              ATA = no;
              SCSI = no;
              EXT4_FS = no;
              BTRFS_FS = no;
              SQUASHFS = no;
              EFI = no;
              CMA = no;
              NUMA = no;
              SWAP = no;
              SUSPEND = no;
              TRANSPARENT_HUGEPAGE = no;
              COMPACTION = no;
              CPU_FREQ = no;
              CPU_IDLE = no;
              HWMON = no;
              THERMAL = no;
              USB_SUPPORT = no;
              DRM = no;
              FB = no;
              INPUT = no;
              SERIO = no;
              VT = no;
              MAGIC_SYSRQ = no;
              NETFILTER = no;
              NET_DSA = no;
              ETHERNET = no;
              WLAN = no;
              AUTOFS_FS = no;
              CRASH_DUMP = no;
              HUGETLBFS = no;
              I2C = no;
              SPI = no;
              WATCHDOG = no;
              PCI_HOST_GENERIC = yes;
              SERIAL_AMBA_PL011 = yes;
              SERIAL_AMBA_PL011_CONSOLE = yes;
              EROFS_FS = yes;
              EROFS_FS_BACKED_BY_FILE = yes;
              FUSE_FS = yes;
              VIRTIO_FS = yes;
              VSOCKETS = yes;
              VIRTIO_VSOCKETS = yes;
              VIRTIO_BLK = yes;
              HW_RANDOM_VIRTIO = yes;
              DEVTMPFS = yes;
              DEVTMPFS_MOUNT = yes;
            };
          };
          # kernel 7.x modules_install no longer creates the build symlink,
          # and MODULES=n never produces Module.symvers, so replace nixpkgs'
          # module-build postInstall with a minimal one
          guest-kernel = guest-kernel-base.overrideAttrs (_: prev: {
            postInstall = ''
              mkdir -p $dev $modules
              cp vmlinux $dev/
            '';
          });
          initramfs = bp.runCommand "initramfs.cpio.gz"
            {
              nativeBuildInputs = [
                bp.cpio
                bp.lz4
              ];
              closure = bp.closureInfo { rootPaths = [ guest-runtime ]; };
            }
            ''
              root=$(mktemp -d)
              mkdir -p $root/nix/store $root/dev $root/proc $root/sys $root/tmp
              while read -r p; do
                cp -R "$p" "$root/nix/store/"
              done < "$closure/store-paths"
              ln -s ${guest-runtime}/bin/guest-runtime $root/init
              (cd "$root" && find . | LC_ALL=C sort | cpio -o -H newc --reproducible | lz4 -l -9 --favor-decSpeed) > $out
            '';
          nix-closure-image = bp.runCommand "nix-closure-image"
            {
              nativeBuildInputs = [ bp.erofs-utils ];
              closure = bp.closureInfo { rootPaths = [ pkgs.nix pkgs.nix.man ]; };
            }
            ''
              root=$(mktemp -d)
              while read -r p; do
                cp -R "$p" "$root/"
              done < "$closure/store-paths"
              mkdir -p $out
              mkfs.erofs -T1 --all-root \
                -U 00000000-0000-0000-0000-000000000000 \
                -L nix-closure \
                $out/nix-closure.erofs "$root"
              echo ${pkgs.nix} > $out/root
            '';
        in
        {
          inherit guest-runtime guest-kernel initramfs nix-closure-image;
        }
      );
    };
}
