#!/bin/sh
# Generates lvm.img.zst, a small LVM2 physical volume for the disk image tests, and
# luks-lvm.img.zst, a second PV wrapped in a LUKS1 volume for the tests of nested layers.
#
# LVM's own tools only work on block devices (which needs root), so this writes the on-disk format
# directly with a short Python script: PV label, metadata area header and the VG text. Needs
# ext4.img.zst (generate-ext.sh) and luks2.img.zst (generate-encrypted.sh), so run those first.
# cryptsetup formats the LUKS1 header of a plain file and encrypt-fixtures.py encrypts the payload,
# like in generate-encrypted.sh. lvm2 is optional and only checks the result:
#
#     nix-shell -p 'python3.withPackages (p: [p.cryptography])' cryptsetup dosfstools mtools zstd lvm2 \
#         --run 'sh tests/fixtures/generate-lvm.sh'
#
# lvm.img is deterministic and re-running leaves lvm.img.zst unchanged. luks-lvm.img changes every
# time (cryptsetup draws salts, key material and the UUID at random, mkfs.fat a volume id); the
# tests do not depend on those bytes.
#
# lvm.img is a bare PV without a partition table (16 MiB, 1 MiB extents, data from 1 MiB on)
# holding the volume group vg0 (checked by tests/backend.rs):
#
#     extent  LV              what
#     0-3     root            linear, the ext4 filesystem of ext4.img.zst
#     4       swap            linear, zeros (no filesystem)
#     5-6     wide            striped over two stripes, which can't be read
#     7-8     pool0_tdata     hidden part of the thin pool
#     9       pool0_tmeta     hidden part of the thin pool
#             pool0           thin pool, which can't be read
#             thin1           thin volume in pool0 (8 MiB), which can't be read
#
# luks-lvm.img is an 18 MiB LUKS1 volume (passphrase "luks fixture", aes-xts-plain64, 512 byte
# sectors, payload at 2 MiB, same parameters as luks1.img) whose plaintext is a PV of the same
# shape (16 MiB, 1 MiB extents, data from 1 MiB on) holding the volume group vg1, all LVs linear:
#
#     extent  LV              what
#     0-3     root            the ext4 filesystem of ext4.img.zst
#     4-5     crypt           luks2.img.zst (LUKS2, passphrase "luks fixture", holding a FAT
#                             volume), 1.25 MiB padded with zeros
#     6       fat             a 1 MiB FAT12 volume LVFAT with one file /hello.txt ("hello lvm fat\n")
#     7       swap            zeros (no filesystem)
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

zstd -d --quiet "$dir/ext4.img.zst" -o "$work/ext4.img"
zstd -d --quiet "$dir/luks2.img.zst" -o "$work/luks2.img"

# The FAT volume of the nested PV.
printf 'hello lvm fat\n' >"$work/hello.txt"
truncate -s 1M "$work/lvfat.img"
mkfs.fat -F 12 -n LVFAT "$work/lvfat.img" >/dev/null
MTOOLS_SKIP_CHECK=1 mcopy -i "$work/lvfat.img" "$work/hello.txt" ::hello.txt

python3 -I - "$work" <<'PY'
import hashlib
import os
import struct
import sys

work = sys.argv[1]
SECTOR = 512
MIB = 1 << 20
TOTAL = 16 * MIB
EXTENT = MIB
PE_START = MIB
MDA_START = 4096
CREATION = 1700000000
VISIBLE = '["READ", "WRITE", "VISIBLE"]'
HIDDEN = '["READ", "WRITE"]'


def lvm_crc(data, crc=0xF597A6CF):
    # LVM's CRC-32: polynomial 0xedb88320, seed 0xf597a6cf, no final xor.
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ 0xEDB88320 if crc & 1 else crc >> 1
    return crc


def uuid(name):
    # 32 characters, written as 6-4-4-4-4-4-6 in the metadata.
    raw = hashlib.md5(name.encode()).hexdigest()
    return raw, "-".join(raw[a:b] for a, b in [(0, 6), (6, 10), (10, 14), (14, 18), (18, 22), (22, 26), (26, 32)])


def linear(start, count, pv_extent):
    return f"""segment1 {{
start_extent = {start}
extent_count = {count}
type = "striped"
stripe_count = 1
stripes = ["pv0", {pv_extent}]
}}"""


def lv(name, status, segment):
    return f"""{name} {{
id = "{uuid("lv-" + name)[1]}"
status = {status}
flags = []
creation_time = {CREATION}
creation_host = "fixture"
segment_count = 1
{segment}
}}
"""


def volume(name, start, count, payload=None, status=VISIBLE, segment=None):
    """One LV: by default linear over the PV extents start..start+count, filled with the file
    payload (if any). Pass segment for other layouts; those carry no payload."""
    return name, status, segment or linear(0, count, start), start, count, payload


def build_pv(out_path, vg_name, volumes):
    lvs = "".join(lv(name, status, segment) for name, status, segment, _, _, _ in volumes)
    pv_raw, pv_id = uuid("pv0")
    text = f"""contents = "Text Format Volume Group"
version = 1
description = ""
creation_host = "fixture"
creation_time = {CREATION}

{vg_name} {{
id = "{uuid(vg_name)[1]}"
seqno = 2
format = "lvm2"
status = ["RESIZEABLE", "READ", "WRITE"]
flags = []
extent_size = {EXTENT // SECTOR}
max_lv = 0
max_pv = 0
metadata_copies = 0

physical_volumes {{
pv0 {{
id = "{pv_id}"
device = "/dev/sda2"
status = ["ALLOCATABLE"]
flags = []
dev_size = {TOTAL // SECTOR}
pe_start = {PE_START // SECTOR}
pe_count = {(TOTAL - PE_START) // EXTENT}
}}
}}

logical_volumes {{
{lvs}}}
}}
""".encode() + b"\0"

    image = bytearray(TOTAL)

    # Sector 1: the PV label, which holds the PV header with its data and metadata areas.
    header = pv_raw.encode() + struct.pack("<Q", TOTAL)
    header += struct.pack("<QQ", PE_START, 0) + bytes(16)  # data area, size 0 = to the end
    header += struct.pack("<QQ", MDA_START, PE_START - MDA_START) + bytes(16)  # metadata area
    label = bytearray(SECTOR)
    label[0:8] = b"LABELONE"
    struct.pack_into("<Q", label, 8, 1)  # this sector's number
    struct.pack_into("<I", label, 20, 32)  # offset of the PV header
    label[24:32] = b"LVM2 001"
    label[32 : 32 + len(header)] = header
    struct.pack_into("<I", label, 16, lvm_crc(bytes(label[20:])))
    image[SECTOR : 2 * SECTOR] = label

    # The metadata area: a header, then the text right behind it (no ring buffer wrap).
    text_at = SECTOR
    mda = bytearray(SECTOR)
    mda[4:20] = b" LVM2 x[5A%r0N*>"
    struct.pack_into("<IQQ", mda, 20, 1, MDA_START, PE_START - MDA_START)
    struct.pack_into("<QQII", mda, 40, text_at, len(text), lvm_crc(text), 0)
    struct.pack_into("<I", mda, 0, lvm_crc(bytes(mda[4:])))
    image[MDA_START : MDA_START + SECTOR] = mda
    image[MDA_START + text_at : MDA_START + text_at + len(text)] = text

    for name, _, _, start, count, payload in volumes:
        if payload is None:
            continue
        with open(payload, "rb") as f:
            data = f.read()
        assert len(data) <= count * EXTENT, f"{name}: {payload} does not fit {count} extents"
        at = PE_START + start * EXTENT
        image[at : at + len(data)] = data
    with open(out_path, "wb") as f:
        f.write(image)


ext4 = os.path.join(work, "ext4.img")
assert os.path.getsize(ext4) == 4 * MIB

build_pv(
    os.path.join(work, "lvm.img"),
    "vg0",
    [
        volume("root", 0, 4, ext4),
        volume("swap", 4, 1),
        volume(
            "wide",
            5,
            2,
            segment="""segment1 {
start_extent = 0
extent_count = 2
type = "striped"
stripe_count = 2
stripe_size = 128
stripes = ["pv0", 5, "pv0", 6]
}""",
        ),
        volume(
            "pool0",
            7,
            2,
            segment="""segment1 {
start_extent = 0
extent_count = 2
type = "thin-pool"
metadata = "pool0_tmeta"
pool = "pool0_tdata"
transaction_id = 1
chunk_size = 128
discards = "passdown"
zero_new_blocks = 1
}""",
        ),
        volume(
            "thin1",
            0,
            8,
            segment="""segment1 {
start_extent = 0
extent_count = 8
type = "thin"
thin_pool = "pool0"
transaction_id = 0
device_id = 1
}""",
        ),
        volume("pool0_tdata", 7, 2, status=HIDDEN),
        volume("pool0_tmeta", 9, 1, status=HIDDEN),
    ],
)

# The plaintext of luks-lvm.img.
build_pv(
    os.path.join(work, "nested.img"),
    "vg1",
    [
        volume("root", 0, 4, ext4),
        volume("crypt", 4, 2, os.path.join(work, "luks2.img")),
        volume("fat", 6, 1, os.path.join(work, "lvfat.img")),
        volume("swap", 7, 1),
    ],
)
PY

if command -v lvm >/dev/null; then
    # Reads the headers and the metadata of the files like LVM would, and checks all CRCs.
    export LVM_SYSTEM_DIR=$work
    for pv in lvm:vg0 nested:vg1; do
        image=$work/${pv%%:*}.img
        vg=${pv#*:}
        lvm pvck --dump headers "$image" 2>/dev/null | grep -q 'LABELONE' || { echo "pvck rejects the PV $image" >&2; exit 1; }
        lvm pvck --dump metadata "$image" 2>/dev/null | grep -q "$vg {" || { echo "pvck rejects the metadata of $image" >&2; exit 1; }
    done
fi
zstd -19 --quiet --force "$work/lvm.img" -o "$dir/lvm.img.zst"

# LUKS1 around the nested PV: 2 MiB header, then the 16 MiB payload.
printf 'luks fixture' >"$work/pass"
truncate -s 18M "$work/luks-lvm.img"
cryptsetup luksFormat --batch-mode --type luks1 --cipher aes-xts-plain64 --key-size 256 \
    --hash sha256 --pbkdf-force-iterations 1000 --key-file "$work/pass" "$work/luks-lvm.img" >/dev/null 2>&1
cryptsetup luksDump --dump-master-key --batch-mode --key-file "$work/pass" "$work/luks-lvm.img" 2>/dev/null |
    sed -n '/^MK dump:/,$p' | sed 's/^MK dump://' | tr -dc '0-9a-f' >"$work/luks-lvm.key"
python3 -I "$dir/encrypt-fixtures.py" luks1 "$work/luks-lvm.img" "$work/nested.img" "$work/luks-lvm.key" 512
zstd -19 --quiet --force "$work/luks-lvm.img" -o "$dir/luks-lvm.img.zst"

ls -l "$dir/lvm.img.zst" "$dir/luks-lvm.img.zst"
