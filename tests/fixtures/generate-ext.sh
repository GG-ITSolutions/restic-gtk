#!/bin/sh
# Generates ext4.img.zst and ext2.img.zst, small ext filesystems for the disk image tests.
#
# Uses `mke2fs -d`, which fills the image from a folder, so no mount and no root:
#
#     nix shell nixpkgs#e2fsprogs nixpkgs#zstd nixpkgs#fakeroot -c sh tests/fixtures/generate-ext.sh
#
# Both images hold the same files (checked by tests/backend.rs); ext2 has no journal and no
# extents, ext4 has both. generate-lvm.sh embeds ext4.img.zst in an LVM volume.
#
#     /docs/readme.txt       "hello ext\n"
#     /docs/größe.txt        "unicode\n"
#     /docs/sub/deep.txt     "deep\n"
#     /empty/
#     /big.bin               300000 bytes, byte i = (i * 7) % 251
#     /sparse.bin            200000 bytes, zeros except "tail" in the last 4
#     /link -> docs/readme.txt
#     /hard.txt              hard link of /docs/readme.txt
#
# All files have mtime 2023-11-14 22:13:20 UTC (1700000000), owner root (via fakeroot).
# The images are reproducible: the same tools produce the same bytes.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

src=$work/src
mkdir -p "$src/docs/sub" "$src/empty"
printf 'hello ext\n' >"$src/docs/readme.txt"
printf 'unicode\n' >"$src/docs/größe.txt"
printf 'deep\n' >"$src/docs/sub/deep.txt"
python3 -I -c 'import sys; sys.stdout.buffer.write(bytes(i * 7 % 251 for i in range(300000)))' >"$src/big.bin"
truncate -s 200000 "$src/sparse.bin"
printf 'tail' | dd of="$src/sparse.bin" bs=1 seek=199996 conv=notrunc status=none
ln -s docs/readme.txt "$src/link"
ln "$src/docs/readme.txt" "$src/hard.txt"
# Folders last, they change when entries are added.
find "$src" -depth -exec touch -h -d @1700000000 {} +

export SOURCE_DATE_EPOCH=1700000000 E2FSPROGS_FAKE_TIME=1700000000
for type in ext4 ext2; do
    img=$work/$type.img
    inode_size=
    [ "$type" = ext4 ] && inode_size="-I 256"
    truncate -s 4M "$img"
    # shellcheck disable=SC2086
    fakeroot mke2fs -q -t "$type" -b 1024 -m 0 $inode_size -L fixture \
        -U 11111111-2222-3333-4444-555555555555 \
        -E hash_seed=66666666-7777-8888-9999-000000000000,root_owner=0:0 \
        -d "$src" "$img"
    e2fsck -fn "$img" >/dev/null 2>&1 || { echo "e2fsck found problems in $img" >&2; exit 1; }
    zstd -19 --quiet --force "$img" -o "$dir/$type.img.zst"
    ls -l "$dir/$type.img.zst"
done
