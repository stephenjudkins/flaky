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
          inherit guest-runtime initramfs;
        }
      );
    };
}
