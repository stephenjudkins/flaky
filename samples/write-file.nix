# Simple single-output derivation: write a file inside $out.
{ pkgs }:
pkgs.runCommand "flaky-sample-write-file" { } ''
  mkdir -p "$out/bin"
  echo "hello from a flaky sample build" > "$out/message.txt"
  printf '#!%s\necho flaky sample\n' "${pkgs.bash}/bin/bash" > "$out/bin/flaky-sample"
  chmod +x "$out/bin/flaky-sample"
''
