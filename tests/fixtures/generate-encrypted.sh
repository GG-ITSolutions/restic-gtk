#!/bin/sh
# Generates luks2.img.zst, luks1.img.zst and bitlocker.img.zst: small encrypted volumes for
# the disk image tests, all holding the same 1 MiB FAT volume.
#
# No mount and no root. cryptsetup formats the LUKS headers of plain files; the payload is
# encrypted by encrypt-fixtures.py, which also builds the BitLocker volume (there is no tool
# to create one without Windows):
#
#     nix-shell -p 'python3.withPackages (p: [p.cryptography])' cryptsetup dosfstools mtools zstd \
#         --run 'sh tests/fixtures/generate-encrypted.sh'
#
# Re-running changes the bytes of the LUKS images (cryptsetup draws salts, key material and the
# UUID at random) and of the FAT volume (volume id, mtimes); only the BitLocker builder is
# deterministic for a given plaintext. The tests do not depend on any of these bytes.
#
# Check the BitLocker volume against an independent implementation (not part of the script):
#
#     nix shell nixpkgs#libbde -c bdeinfo -r 013574-624679-047531-660000-008547-345565-299002-046662 bitlocker.img
#
# Contents of the FAT volume (checked by tests/backend.rs):
#
#     /hello.txt             "hello encrypted\n"
#     /docs/readme.txt       "readme\n"
#     /docs/sub/deep.txt     "deep\n"
#     /pattern.bin           20000 bytes, byte i = (i * 7) % 251
#
# Keys (test data, checked in on purpose):
#
#     luks2.img, luks1.img   passphrase "luks fixture"
#     bitlocker.img          recovery password 013574-624679-047531-660000-008547-345565-299002-046662
#                            or password "bitlocker fixture"
set -eu

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The plaintext: FAT12, its files all within the first 64 KiB (BitLocker keeps its metadata
# blocks at 64 KiB and above).
src=$work/src
mkdir -p "$src/docs/sub"
printf 'hello encrypted\n' >"$src/hello.txt"
printf 'readme\n' >"$src/docs/readme.txt"
printf 'deep\n' >"$src/docs/sub/deep.txt"
python3 -I -c 'import sys; sys.stdout.buffer.write(bytes(i * 7 % 251 for i in range(20000)))' >"$src/pattern.bin"
truncate -s 1M "$work/plain.img"
mkfs.fat -F 12 -n CRYPT "$work/plain.img" >/dev/null
MTOOLS_SKIP_CHECK=1 mcopy -i "$work/plain.img" -s "$src"/* ::

# LUKS2: argon2id, 4096 byte sectors, payload at 256 KiB.
printf 'luks fixture' >"$work/pass"
truncate -s 1280K "$work/luks2.img"
cryptsetup luksFormat --batch-mode --type luks2 --cipher aes-xts-plain64 --key-size 256 \
    --hash sha256 --pbkdf argon2id --pbkdf-memory 1024 --pbkdf-parallel 2 --pbkdf-force-iterations 4 \
    --sector-size 4096 --luks2-metadata-size 16k --luks2-keyslots-size 128k --offset 512 \
    --key-file "$work/pass" "$work/luks2.img" >/dev/null 2>&1
cryptsetup luksDump --dump-master-key --batch-mode --key-file "$work/pass" "$work/luks2.img" 2>/dev/null |
    sed -n '/^MK dump:/,$p' | sed 's/^MK dump://' | tr -dc '0-9a-f' >"$work/luks2.key"
python3 -I "$here/encrypt-fixtures.py" luks2 "$work/luks2.img" "$work/plain.img" "$work/luks2.key" 4096 262144

# LUKS1: pbkdf2-sha256, 512 byte sectors.
truncate -s 4M "$work/luks1.img"
cryptsetup luksFormat --batch-mode --type luks1 --cipher aes-xts-plain64 --key-size 256 \
    --hash sha256 --pbkdf-force-iterations 1000 --key-file "$work/pass" "$work/luks1.img" >/dev/null 2>&1
cryptsetup luksDump --dump-master-key --batch-mode --key-file "$work/pass" "$work/luks1.img" 2>/dev/null |
    sed -n '/^MK dump:/,$p' | sed 's/^MK dump://' | tr -dc '0-9a-f' >"$work/luks1.key"
python3 -I "$here/encrypt-fixtures.py" luks1 "$work/luks1.img" "$work/plain.img" "$work/luks1.key" 512

python3 -I "$here/encrypt-fixtures.py" bitlocker "$work/plain.img" "$work/bitlocker.img"

for name in luks2 luks1 bitlocker; do
    zstd -19 --quiet --force "$work/$name.img" -o "$here/$name.img.zst"
done
ls -l "$here"/luks2.img.zst "$here"/luks1.img.zst "$here"/bitlocker.img.zst
