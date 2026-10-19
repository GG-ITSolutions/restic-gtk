#!/usr/bin/env python3
"""Generates WOF test vectors with wimlib's compressors.

Writes `plain.bin` and one `WofCompressedData` stream per WOF format: a chunk
offset table followed by the chunks, each chunk compressed on its own and
stored raw where compression doesn't make it smaller, as Windows does.

Run from the repository root:

    nix shell nixpkgs#wimlib nixpkgs#python3 -c \
        python3 tests/fixtures/codecs/generate.py
"""

import ctypes
import ctypes.util
import os
import random
import struct

HERE = os.path.dirname(os.path.abspath(__file__))

XPRESS = 1
LZX = 2
FORMATS = [
    ("xpress4k", XPRESS, 4096),
    ("xpress8k", XPRESS, 8192),
    ("xpress16k", XPRESS, 16384),
    ("lzx", LZX, 32768),
]


def load_wimlib():
    for name in [ctypes.util.find_library("wim"), "libwim.so.15", "libwim.so"]:
        if name:
            try:
                return ctypes.CDLL(name)
            except OSError:
                pass
    # nix shell puts the library next to the binaries, not on the search path.
    for path in os.environ.get("PATH", "").split(":"):
        candidate = os.path.join(os.path.dirname(path), "lib", "libwim.so")
        if os.path.exists(candidate):
            return ctypes.CDLL(candidate)
    raise SystemExit("libwim not found, run inside `nix shell nixpkgs#wimlib`")


def plain_data():
    """Text, an incompressible stretch and x86-like code with E8 calls."""
    rng = random.Random(15)
    words = ["restic", "snapshot", "partition", "ntfs", "chunk", "windows", "größe"]
    text = " ".join(rng.choice(words) for _ in range(5000)).encode()
    noise = bytes(rng.getrandbits(8) for _ in range(34000))
    code = bytearray()
    for i in range(2500):
        code += b"\x55\x8b\xec\xe8" + struct.pack("<i", rng.randrange(-5000, 5000)) + bytes([i % 7])
    zeros = bytes(5000)
    # Not a multiple of any chunk size, so the last chunk is short.
    return (text + noise + bytes(code) + zeros + text[:1234])[:110_000]


def compress_stream(lib, ctype, chunk_size, data):
    compressor = ctypes.c_void_p()
    if lib.wimlib_create_compressor(ctype, ctypes.c_size_t(chunk_size), 0, ctypes.byref(compressor)):
        raise SystemExit("wimlib_create_compressor failed")
    lib.wimlib_compress.restype = ctypes.c_size_t
    chunks = []
    for start in range(0, len(data), chunk_size):
        chunk = data[start : start + chunk_size]
        out = ctypes.create_string_buffer(len(chunk))
        size = lib.wimlib_compress(
            chunk, ctypes.c_size_t(len(chunk)), out, ctypes.c_size_t(len(chunk) - 1), compressor
        )
        chunks.append(out.raw[:size] if 0 < size < len(chunk) else chunk)
    lib.wimlib_free_compressor(compressor)

    # Offsets of chunks 1.. from the end of the table; 4 bytes for files < 4 GiB.
    table = b""
    offset = 0
    for chunk in chunks[:-1]:
        offset += len(chunk)
        table += struct.pack("<I", offset)
    return table + b"".join(chunks), sum(len(c) == chunk_size for c in chunks)


def main():
    lib = load_wimlib()
    data = plain_data()
    with open(os.path.join(HERE, "plain.bin"), "wb") as f:
        f.write(data)
    for name, ctype, chunk_size in FORMATS:
        stream, raw = compress_stream(lib, ctype, chunk_size, data)
        with open(os.path.join(HERE, f"{name}.wof"), "wb") as f:
            f.write(stream)
        print(f"{name}: {len(stream)} bytes, {raw} chunks stored raw")


if __name__ == "__main__":
    main()
