#!/bin/sh
# Generates lvm.img.zst, a small LVM2 physical volume for the disk image tests.
#
# LVM's own tools only work on block devices (which needs root), so this writes the on-disk format
# directly with a short Python script: PV label, metadata area header and the VG text. Needs
# ext4.img.zst, so run generate-ext.sh first. lvm2 is optional and only checks the result:
#
#     nix shell nixpkgs#zstd nixpkgs#lvm2 -c sh tests/fixtures/generate-lvm.sh
#
# The image is a bare PV without a partition table (16 MiB, 1 MiB extents, data from 1 MiB on)
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
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

zstd -d --quiet "$dir/ext4.img.zst" -o "$work/ext4.img"
python3 -I - "$work/ext4.img" "$work/lvm.img" <<'PY'
import hashlib
import struct
import sys

ext4_path, out_path = sys.argv[1:3]
SECTOR = 512
MIB = 1 << 20
TOTAL = 16 * MIB
EXTENT = MIB
PE_START = MIB
MDA_START = 4096
CREATION = 1700000000


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


VISIBLE = '["READ", "WRITE", "VISIBLE"]'
HIDDEN = '["READ", "WRITE"]'
lvs = "".join(
    [
        lv("root", VISIBLE, linear(0, 4, 0)),
        lv("swap", VISIBLE, linear(0, 1, 4)),
        lv(
            "wide",
            VISIBLE,
            """segment1 {
start_extent = 0
extent_count = 2
type = "striped"
stripe_count = 2
stripe_size = 128
stripes = ["pv0", 5, "pv0", 6]
}""",
        ),
        lv(
            "pool0",
            VISIBLE,
            """segment1 {
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
        lv(
            "thin1",
            VISIBLE,
            """segment1 {
start_extent = 0
extent_count = 8
type = "thin"
thin_pool = "pool0"
transaction_id = 0
device_id = 1
}""",
        ),
        lv("pool0_tdata", HIDDEN, linear(0, 2, 7)),
        lv("pool0_tmeta", HIDDEN, linear(0, 1, 9)),
    ]
)

pv_raw, pv_id = uuid("pv0")
text = f"""contents = "Text Format Volume Group"
version = 1
description = ""
creation_host = "fixture"
creation_time = {CREATION}

vg0 {{
id = "{uuid("vg0")[1]}"
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

with open(ext4_path, "rb") as f:
    fs = f.read()
assert len(fs) == 4 * MIB
image[PE_START : PE_START + len(fs)] = fs
with open(out_path, "wb") as f:
    f.write(image)
PY

if command -v lvm >/dev/null; then
    # Reads the headers and the metadata of the file like LVM would, and checks all CRCs.
    export LVM_SYSTEM_DIR=$work
    lvm pvck --dump headers "$work/lvm.img" 2>/dev/null | grep -q 'LABELONE' || { echo "pvck rejects the PV" >&2; exit 1; }
    lvm pvck --dump metadata "$work/lvm.img" 2>/dev/null | grep -q 'vg0 {' || { echo "pvck rejects the metadata" >&2; exit 1; }
fi
zstd -19 --quiet --force "$work/lvm.img" -o "$dir/lvm.img.zst"
ls -l "$dir/lvm.img.zst"
