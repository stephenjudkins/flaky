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
          guest-init = pkgs.rustPlatform.buildRustPackage {
            pname = "guest-init";
            version = "0.1.0";
            src = ./guest-init;
            cargoLock.lockFile = ./guest-init/Cargo.lock;
          };
          initramfs = bp.runCommand "initramfs.cpio.gz"
            {
              nativeBuildInputs = [
                bp.cpio
                bp.gzip
              ];
              closure = bp.closureInfo { rootPaths = [ guest-init ]; };
            }
            ''
              root=$(mktemp -d)
              mkdir -p $root/nix/store $root/dev $root/proc $root/sys $root/tmp
              while read -r p; do
                cp -R "$p" "$root/nix/store/"
              done < "$closure/store-paths"
              ln -s ${guest-init}/bin/guest-init $root/init
              (cd "$root" && find . | LC_ALL=C sort | cpio -o -H newc --reproducible | gzip -9n) > $out
            '';
        in
        {
          inherit guest-init initramfs;
        }
      );
    };
}
