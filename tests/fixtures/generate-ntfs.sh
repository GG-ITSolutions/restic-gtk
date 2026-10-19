#!/bin/sh
# Generates ntfs.img.zst, a small NTFS volume for the disk image tests.
#
# Uses mkntfs and wimlib's NTFS-3G mode, so no mount and no root:
#
#     nix shell nixpkgs#ntfs3g nixpkgs#wimlib nixpkgs#zstd -c sh tests/fixtures/generate-ntfs.sh
#
# Contents (checked by tests/backend.rs):
#
#     /docs/readme.txt       "hello ntfs\n"
#     /docs/größe.txt        "unicode\n"
#     /docs/sub/deep.txt     "deep\n"
#     /empty/
#     /big.bin               300000 bytes, byte i = (i * 7) % 251
#     /link -> docs          directory symlink
set -eu

out=$(cd "$(dirname "$0")" && pwd)/ntfs.img.zst
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

src=$work/src
mkdir -p "$src/docs/sub" "$src/empty"
printf 'hello ntfs\n' >"$src/docs/readme.txt"
printf 'unicode\n' >"$src/docs/größe.txt"
printf 'deep\n' >"$src/docs/sub/deep.txt"
python3 -I -c 'import sys; sys.stdout.buffer.write(bytes(i * 7 % 251 for i in range(300000)))' >"$src/big.bin"
ln -s docs "$src/link"

truncate -s 4M "$work/ntfs.img"
mkntfs --force --quick --quiet --label Fixture "$work/ntfs.img"
wimcapture "$src" "$work/src.wim" >/dev/null
wimapply "$work/src.wim" 1 "$work/ntfs.img" >/dev/null
zstd -19 --quiet --force "$work/ntfs.img" -o "$out"
ls -l "$out"
