#!/bin/sh
set -eu
cd "$(dirname "$0")"

rm -f guest/vmlinux.bin
nix build .#packages.aarch64-linux.guest-kernel -o guest/kernel
ln -s kernel/Image guest/vmlinux.bin

echo "built guest/vmlinux.bin"
