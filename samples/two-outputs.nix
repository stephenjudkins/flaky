# Multi-output derivation: exercises several output block devices in one VM.
{ pkgs }:
pkgs.runCommand "flaky-sample-two-outputs"
  { outputs = [ "out" "message" ]; }
  ''
    mkdir -p "$out/bin" "$message"
    echo "flaky multi-output body" > "$out/body.txt"
    printf '#!%s\necho out\n' "${pkgs.bash}/bin/bash" > "$out/bin/two-outputs"
    chmod +x "$out/bin/two-outputs"
    echo "flaky multi-output message" > "$message/note.txt"
  ''
