# Output is a plain file at $out, not a directory: common in trivial
# builders (writeText-style).
{ pkgs }:
pkgs.runCommand "flaky-sample-file-output" { } ''
  echo "just a file output" > $out
''
