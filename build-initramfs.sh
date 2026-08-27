#!/bin/sh
set -eu
cd "$(dirname "$0")"

rm -f guest/initramfs.cpio.gz
nix build .#packages.aarch64-linux.initramfs -o guest/initramfs.cpio.gz

echo "built guest/initramfs.cpio.gz"
