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
              DEVTMPFS = yes;
              DEVTMPFS_MOUNT = yes;
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
                bp.gzip
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
              (cd "$root" && find . | LC_ALL=C sort | cpio -o -H newc --reproducible | gzip -9n) > $out
            '';
        in
        {
          inherit guest-runtime guest-kernel initramfs;
        }
      );
    };
}
