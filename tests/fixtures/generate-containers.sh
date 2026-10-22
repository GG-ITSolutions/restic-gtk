#!/bin/sh
# Generates containers/*.zst, the fixtures for the VM disk format tests: the NTFS volume of
# ntfs.img.zst converted to qcow2, VHD, VHDX and VMDK, each compressed with zstd, and
# nested.qcow2.zst, the LUKS1 volume of luks-lvm.img.zst (generate-lvm.sh) as a plain qcow2
# for the tests of nested layers.
#
# Uses qemu-img, no mount and no root:
#
#     nix shell nixpkgs#qemu-utils nixpkgs#zstd -c sh tests/fixtures/generate-containers.sh [all|ntfs|nested]
#
# Without an argument (or with "all") it writes every file; "ntfs" only the NTFS based containers
# and "nested" only nested.qcow2.zst. qemu-img writes new UUIDs and timestamps, so running this
# again changes every file it writes.
set -eu

mode=${1:-all}
case $mode in
all | ntfs | nested) ;;
*) echo "usage: $0 [all|ntfs|nested]" >&2; exit 2 ;;
esac

dir=$(cd "$(dirname "$0")" && pwd)
out=$dir/containers
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"
cd "$work"

# convert SOURCE NAME ARGS...: converts the raw image SOURCE with the qemu-img options ARGS to NAME.
convert() {
    source=$1
    name=$2
    shift 2
    qemu-img convert -f raw "$@" "$source" "$name"
    pack "$name"
}

pack() {
    zstd -19 --quiet --force "$1" -o "$out/$1.zst"
}

if [ "$mode" != nested ]; then
    zstd -d --quiet "$dir/ntfs.img.zst" -o ntfs.img
    convert ntfs.img plain.qcow2 -O qcow2
    convert ntfs.img zlib.qcow2 -O qcow2 -c
    convert ntfs.img zstd.qcow2 -O qcow2 -c -o compression_type=zstd
    convert ntfs.img fixed.vhd -O vpc -o subformat=fixed
    convert ntfs.img dynamic.vhd -O vpc -o subformat=dynamic
    convert ntfs.img fixed.vhdx -O vhdx -o subformat=fixed
    convert ntfs.img dynamic.vhdx -O vhdx -o subformat=dynamic
    convert ntfs.img sparse.vmdk -O vmdk -o subformat=monolithicSparse
    convert ntfs.img stream.vmdk -O vmdk -o subformat=streamOptimized

    # Images that need a parent; browsing them is refused.
    qemu-img create --quiet -f qcow2 -b ntfs.img -F raw backing.qcow2
    pack backing.qcow2
    qemu-img create --quiet -f vmdk -b sparse.vmdk -F vmdk child.vmdk
    pack child.vmdk
fi

if [ "$mode" != ntfs ]; then
    zstd -d --quiet "$dir/luks-lvm.img.zst" -o luks-lvm.img
    convert luks-lvm.img nested.qcow2 -O qcow2
fi

ls -l "$out"
