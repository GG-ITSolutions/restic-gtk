#!/bin/sh
# Generates containers/*.zst, the fixtures for the VM disk format tests: the NTFS volume of
# ntfs.img.zst converted to qcow2, VHD, VHDX and VMDK, each compressed with zstd.
#
# Uses qemu-img, no mount and no root:
#
#     nix shell nixpkgs#qemu-utils nixpkgs#zstd -c sh tests/fixtures/generate-containers.sh
#
# qemu-img writes new UUIDs and timestamps, so running this again changes every file.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
out=$dir/containers
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

zstd -d --quiet "$dir/ntfs.img.zst" -o "$work/ntfs.img"
cd "$work"

# convert NAME ARGS...: converts ntfs.img with the qemu-img options ARGS to NAME.
convert() {
    name=$1
    shift
    qemu-img convert -f raw "$@" ntfs.img "$name"
    pack "$name"
}

pack() {
    zstd -19 --quiet --force "$1" -o "$out/$1.zst"
}

convert plain.qcow2 -O qcow2
convert zlib.qcow2 -O qcow2 -c
convert zstd.qcow2 -O qcow2 -c -o compression_type=zstd
convert fixed.vhd -O vpc -o subformat=fixed
convert dynamic.vhd -O vpc -o subformat=dynamic
convert fixed.vhdx -O vhdx -o subformat=fixed
convert dynamic.vhdx -O vhdx -o subformat=dynamic
convert sparse.vmdk -O vmdk -o subformat=monolithicSparse
convert stream.vmdk -O vmdk -o subformat=streamOptimized

# Images that need a parent; browsing them is refused.
qemu-img create --quiet -f qcow2 -b ntfs.img -F raw backing.qcow2
pack backing.qcow2
qemu-img create --quiet -f vmdk -b sparse.vmdk -F vmdk child.vmdk
pack child.vmdk

ls -l "$out"
