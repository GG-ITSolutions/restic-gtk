#!/usr/bin/env python3
"""Helpers of generate-encrypted.sh: writes encrypted payloads and builds a BitLocker volume.

Run through the shell script (it provides cryptography); not meant to be called by hand:

    encrypt-fixtures.py luks1 IMAGE PLAIN MASTERKEY_HEX SECTOR_SIZE
    encrypt-fixtures.py luks2 IMAGE PLAIN MASTERKEY_HEX SECTOR_SIZE PAYLOAD_OFFSET
        Encrypts PLAIN with aes-xts-plain64 into the payload of the cryptsetup-formatted
        IMAGE. LUKS1 states the payload offset in its header; for LUKS2 pass the bytes given
        to cryptsetup --offset. All-zero sectors above 64 KiB stay zero (not encrypted) so the
        image compresses; the filesystem never reads them.
    encrypt-fixtures.py bitlocker PLAIN OUT
        Writes OUT, PLAIN as a BitLocker volume (Windows 7+ layout, XTS-AES-128) with a
        recovery password and a user password protector. Prints both.
"""
import hashlib
import struct
import sys
import uuid
import zlib

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.ciphers.aead import AESCCM

SECTOR = 512
# The filesystem structures (boot sector, FATs, root folder) lie below this offset; all of it is
# encrypted. Beyond it, all-zero sectors stay zero: they compress, and nothing reads them.
KEEP = 64 * 1024

# BitLocker fixture parameters. The recovery password is eight groups of six digits, each
# divisible by 11 whose quotient fits 16 bits; the user password is plain text.
RECOVERY_WORDS = [1234, 56789, 4321, 60000, 777, 31415, 27182, 4242]
RECOVERY_PASSWORD = "-".join(f"{w * 11:06d}" for w in RECOVERY_WORDS)
USER_PASSWORD = "bitlocker fixture"

STRETCH_ROUNDS = 0x100000
METHOD_XTS128 = 0x8004
HEADER_SECTORS = 16  # relocated to the volume header block
META_OFFSETS = [0x10000, 0x20000, 0x30000]
RELOCATED_OFFSET = 0x40000
BITLOCKER_GUID = bytes.fromhex("3bd66749292ed84a8399f6a339e3d001")  # 4967d63b-2e29-4ad8-...
FILETIME = 0x01DC_0000_0000_0000


def xts(key, data, sector_number, encrypt=True):
    """AES-XTS of one sector; the tweak is the sector number, little-endian."""
    cipher = Cipher(algorithms.AES(key), modes.XTS(sector_number.to_bytes(16, "little")))
    op = cipher.encryptor() if encrypt else cipher.decryptor()
    return op.update(data) + op.finalize()


def encrypt_luks(kind, image_path, plain_path, key_hex, sector_size, payload_offset=None):
    key = bytes.fromhex(open(key_hex).read().strip())
    plain = open(plain_path, "rb").read()
    with open(image_path, "r+b") as image:
        header = image.read(4096)
        if kind == "luks1":
            offset = struct.unpack_from(">I", header, 104)[0] * SECTOR
            # cryptsetup fills unused key slot areas with random bytes: zero them, they are disabled.
            for slot in range(8):
                base = 208 + 48 * slot
                active, _, _salt, material, stripes = struct.unpack_from(">II32sII", header, base)[:5]
                if active != 0x00AC71F3:
                    length = -(-struct.unpack_from(">I", header, 108)[0] * stripes // SECTOR) * SECTOR
                    image.seek(material * SECTOR)
                    image.write(bytes(length))
        else:
            offset = payload_offset
        for index in range(0, len(plain) // sector_size):
            chunk = plain[index * sector_size:(index + 1) * sector_size]
            if not any(chunk) and index * sector_size >= KEEP:
                continue
            # dm-crypt plain64: the tweak counts 512-byte units, also for larger sectors.
            image.seek(offset + index * sector_size)
            image.write(xts(key, chunk, index * (sector_size // SECTOR)))


def ccm_wrap(key, nonce, plain):
    """BitLocker's AES-CCM key layout: nonce, 16 byte tag, ciphertext."""
    sealed = AESCCM(key, tag_length=16).encrypt(nonce, plain, None)
    return nonce + sealed[-16:] + sealed[:-16]


def entry(entry_type, value_type, data):
    return struct.pack("<HHHH", 8 + len(data), entry_type, value_type, 1) + data


def stretch(initial, salt):
    last = bytes(32)
    for count in range(STRETCH_ROUNDS):
        last = hashlib.sha256(last + initial + salt + struct.pack("<Q", count)).digest()
    return last


def recovery_hash():
    return hashlib.sha256(b"".join(struct.pack("<H", w) for w in RECOVERY_WORDS)).digest()


def password_hash(password):
    return hashlib.sha256(hashlib.sha256(password.encode("utf-16-le")).digest()).digest()


def protector(guid, protection, method, initial, salt, nonce, vmk):
    container = struct.pack("<IHHI", 44, 1, 0, 0x2003) + vmk
    stretch_entry = entry(0, 3, struct.pack("<I", method) + salt)
    key_entry = entry(0, 5, ccm_wrap(stretch(initial, salt), nonce, container))
    head = guid + struct.pack("<QHH", FILETIME, 0, protection)
    return entry(2, 8, head + stretch_entry + key_entry)


def build_bitlocker(plain):
    size = len(plain)
    assert size % SECTOR == 0
    assert not any(plain[META_OFFSETS[0]:RELOCATED_OFFSET + HEADER_SECTORS * SECTOR]), \
        "the filesystem must stay out of the area of the metadata blocks"

    def derived(label):
        return hashlib.sha256(b"restic-gtk fixture " + label).digest()

    fvek = derived(b"fvek")  # XTS-AES-128: two different 16 byte keys
    vmk = derived(b"vmk")
    volume_guid = uuid.UUID("0b1f6d3a-5c1e-4f7e-9a53-2f5a4d2d7c11").bytes_le

    # 12 bytes of header, then the key: 32 bytes for XTS-AES-128 (76 in total for XTS-256 and diffuser).
    fvek_container = struct.pack("<IHHI", 44, 1, 0, METHOD_XTS128) + fvek
    entries = b"".join([
        entry(0xF, 0xF, struct.pack("<QQ", RELOCATED_OFFSET, HEADER_SECTORS * SECTOR)),
        entry(7, 2, "restic-gtk fixture".encode("utf-16-le") + b"\0\0"),
        protector(derived(b"guid1")[:16], 0x2000, 0x1001, password_hash(USER_PASSWORD),
                  derived(b"salt1")[:16], derived(b"nonce1")[:12], vmk),
        protector(derived(b"guid2")[:16], 0x0800, 0x1000, recovery_hash(),
                  derived(b"salt2")[:16], derived(b"nonce2")[:12], vmk),
        entry(3, 5, ccm_wrap(vmk, derived(b"nonce3")[:12], fvek_container)),
    ])
    metadata_size = 48 + len(entries)
    block = bytearray(-(-(64 + metadata_size) // 16) * 16)
    block[0:8] = b"-FVE-FS-"
    struct.pack_into("<HHHH", block, 8, len(block) // 16, 2, 4, 4)
    struct.pack_into("<QII", block, 16, size, 0, HEADER_SECTORS)
    struct.pack_into("<3QQ", block, 32, *META_OFFSETS, RELOCATED_OFFSET)
    struct.pack_into("<IIII", block, 64, metadata_size, 1, 48, metadata_size)
    block[80:96] = volume_guid
    struct.pack_into("<IHHQ", block, 96, 4, METHOD_XTS128, 0, FILETIME)
    block[112:112 + len(entries)] = entries
    # After the block: its CRC-32 and its SHA-256 (as a key wrapped with the VMK).
    check = struct.pack("<IHHI", 44, 1, 0, 0x2003) + hashlib.sha256(block).digest()
    check_entry = entry(0, 5, ccm_wrap(vmk, derived(b"nonce4")[:12], check))
    block += struct.pack("<HHI", 8 + len(check_entry), 2, zlib.crc32(block)) + check_entry

    out = bytearray(size)
    # The first sector is the BitLocker boot sector, the others of the header are stale.
    boot = out[0:SECTOR]
    boot[0:3] = b"\xeb\x58\x90"
    boot[3:11] = b"-FVE-FS-"
    boot[11:36] = plain[11:36]  # BPB: sector and cluster size, media, geometry
    struct.pack_into("<I", boot, 32, size // SECTOR)
    boot[160:176] = BITLOCKER_GUID
    struct.pack_into("<3Q", boot, 176, *META_OFFSETS)
    boot[510:512] = b"\x55\xaa"
    out[0:SECTOR] = boot
    for offset in META_OFFSETS:
        out[offset:offset + len(block)] = block
    for index in range(HEADER_SECTORS):  # the real header, moved and encrypted
        sector = plain[index * SECTOR:(index + 1) * SECTOR]
        physical = RELOCATED_OFFSET + index * SECTOR
        out[physical:physical + SECTOR] = xts(fvek, sector, physical // SECTOR)
    for index in range(HEADER_SECTORS, size // SECTOR):
        sector = plain[index * SECTOR:(index + 1) * SECTOR]
        if any(sector) or index * SECTOR < KEEP:
            out[index * SECTOR:(index + 1) * SECTOR] = xts(fvek, sector, index)
    return bytes(out)


def main():
    kind = sys.argv[1]
    if kind == "luks1":
        encrypt_luks(kind, sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]))
    elif kind == "luks2":
        encrypt_luks(kind, sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]), int(sys.argv[6]))
    elif kind == "bitlocker":
        plain = open(sys.argv[2], "rb").read()
        open(sys.argv[3], "wb").write(build_bitlocker(plain))
        print(f"recovery password {RECOVERY_PASSWORD}, password {USER_PASSWORD!r}")
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
