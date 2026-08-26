#!/bin/sh
set -eu
cd "$(dirname "$0")"

cargo build --release --bin oci2erofs
./target/release/oci2erofs nixos-nix-oci guest/nixdisk.erofs

# repack the initramfs so /init knows how to mount the erofs disk
(
  cd initramfs
  find . | cpio -o -H newc 2>/dev/null | gzip -9
) > guest/initramfs.cpio.gz

echo "built guest/nixdisk.erofs and guest/initramfs.cpio.gz"
