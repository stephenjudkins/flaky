{
  description = "flaky build samples";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/2c423e03bbafcff28bfadc6781a4a8257f205cb5";
  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      forLinuxSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forLinuxSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          call = name: pkgs.callPackage ./${name}.nix { };
        in
        {
          write-file = call "write-file";
          two-outputs = call "two-outputs";
          raw-derivation = call "raw-derivation";
          file-output = call "file-output";
        });
    };
}
