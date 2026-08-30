# Raw derivation with a bash builder, no stdenv: minimal input closure
# and no structured attrs.
{ pkgs }:
derivation {
  name = "flaky-sample-raw";
  system = "aarch64-linux";
  builder = "${pkgs.bash}/bin/bash";
  args = [
    "-c"
    "${pkgs.coreutils}/bin/mkdir $out && echo built by raw derivation > $out/raw.txt"
  ];
}
