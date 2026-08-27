#!/bin/sh
set -eu
cd "$(dirname "$0")"

cargo build --release --bin oci2erofs
./target/release/oci2erofs nixos-nix-oci guest/nixdisk.erofs

echo "built guest/nixdisk.erofs (initramfs is built by build-initramfs.sh)"
