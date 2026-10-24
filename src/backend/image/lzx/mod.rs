//! LZX decompression in the variant of WIM archives and WOF-compressed files.
//!
//! The bitstream, Huffman trees and block bodies (`bitstream.rs`, `tree.rs`,
//! `block.rs`) are taken from the [`lzxd`](https://codeberg.org/Lonami/lzxd) crate
//! 0.2.7, © 2020 Lonami, MIT (see `LICENSE-MIT` next to this file). `lzxd`
//! decodes the CAB flavour of LZX; WIM and WOF differ in four points:
//!
//! 1. No stream header: CAB starts with an E8 bit and a translation size.
//! 2. A block size is one bit for the default size 32768, else 16 bits (CAB: 24 bits).
//! 3. The E8 filter is always on, with a translation size of 12,000,000 and
//!    positions counted from the start of the chunk.
//! 4. Every chunk (32 KiB uncompressed, the last one shorter) is compressed on its
//!    own, so trees, repeated offsets and the window start fresh per chunk.
//!
//! Since a chunk never refers to an earlier one, the output buffer is the window.

mod bitstream;
mod block;
mod tree;

use std::fmt;

use bitstream::Bitstream;
use block::{Block, Decoded, Kind as BlockKind};
use tree::{CanonicalTree, Tree};

/// Uncompressed size of a chunk, except for the last one of a file.
pub const CHUNK_SIZE: usize = 32 * 1024;

/// Position slots of a 32 KiB window.
const POSITION_SLOTS: usize = 30;

/// The fixed E8 translation size of WIM and WOF.
const E8_TRANSLATION_SIZE: i32 = 12_000_000;

/// Why a chunk could not be decompressed. The input is corrupt in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFailed {
    /// The chunk data caused a read of more items than the current block had in a single step.
    OverreadBlock,
    /// There was not enough data in the chunk to fully decode, and a premature end was found.
    UnexpectedEof,
    /// An invalid block type was found.
    InvalidBlock(u8),
    /// An invalid block size was found.
    InvalidBlockSize(u32),
    /// An invalid pretree element was found.
    InvalidPretreeElement(u16),
    /// Invalid pretree run-length encoding.
    InvalidPretreeRle,
    /// When attempting to construct a decode tree, we encountered an invalid path length tree.
    InvalidPathLengths,
    /// A required decode tree was empty (all path lengths were 0).
    EmptyTree,
    /// A match refers to data before the start of the chunk.
    InvalidOffset,
    /// The chunk is larger than [`CHUNK_SIZE`].
    ChunkTooLong,
}

impl fmt::Display for DecodeFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use DecodeFailed::*;

        match self {
            OverreadBlock => write!(
                f,
                "read more items than available in the block in a single step"
            ),
            UnexpectedEof => write!(f, "reached end of chunk without fully decoding it"),
            InvalidBlock(kind) => write!(f, "block type {kind} is invalid"),
            InvalidBlockSize(size) => write!(f, "block size {size} is invalid"),
            InvalidPretreeElement(elem) => write!(f, "found invalid pretree element {elem}"),
            InvalidPretreeRle => write!(f, "found invalid pretree rle element"),
            InvalidPathLengths => write!(f, "encountered invalid path lengths"),
            EmptyTree => write!(f, "encountered empty decode tree"),
            InvalidOffset => write!(f, "match refers to data before the chunk"),
            ChunkTooLong => write!(f, "chunk is longer than {CHUNK_SIZE} bytes"),
        }
    }
}

impl std::error::Error for DecodeFailed {}

/// Decoder state carried from block to block within a chunk.
pub(crate) struct DecoderState {
    /// This tree cannot be used directly, it exists only to apply the delta of upcoming trees
    /// to its path lengths.
    main_tree: CanonicalTree,

    /// This tree cannot be used directly, it exists only to apply the delta of upcoming trees
    /// to its path lengths.
    length_tree: CanonicalTree,
}

/// Decompresses one chunk to `output_len` bytes.
pub fn decompress_chunk(chunk: &[u8], output_len: usize) -> Result<Vec<u8>, DecodeFailed> {
    if output_len > CHUNK_SIZE {
        return Err(DecodeFailed::ChunkTooLong);
    }

    let mut bitstream = Bitstream::new(chunk);
    let mut state = DecoderState {
        // > The main tree comprises 256 elements that correspond to all possible 8-bit
        // > characters, plus 8 * NUM_POSITION_SLOTS elements that correspond to matches.
        main_tree: CanonicalTree::new(256 + 8 * POSITION_SLOTS),
        // > The length tree comprises 249 elements.
        length_tree: CanonicalTree::new(249),
    };
    // > The initial state of R0, R1, R2 is (1, 1, 1).
    let mut r = [1, 1, 1];
    let mut block = Block {
        remaining: 0,
        size: 0,
        kind: BlockKind::Uncompressed { r },
    };
    let mut output = Vec::with_capacity(output_len);

    while output.len() < output_len {
        if block.remaining == 0 {
            // An uncompressed block of odd size is padded to a 16-bit boundary.
            if matches!(block.kind, BlockKind::Uncompressed { .. }) && !block.size.is_multiple_of(2)
            {
                bitstream.read_byte();
            }
            block = Block::read(&mut bitstream, &mut state)?;
        }

        let advance = match block.decode_element(&mut bitstream, &mut r)? {
            Decoded::Single(value) => {
                output.push(value);
                1
            }
            Decoded::Match { offset, length } => {
                if offset == 0 || offset > output.len() {
                    return Err(DecodeFailed::InvalidOffset);
                }
                // Byte by byte, matches may overlap their own output.
                for _ in 0..length {
                    output.push(output[output.len() - offset]);
                }
                length
            }
            Decoded::Read(length) => {
                let length = length
                    .min(bitstream.remaining_bytes())
                    .min(output_len - output.len());
                let start = output.len();
                output.resize(start + length, 0);
                bitstream.read_raw(&mut output[start..])?;
                length
            }
        };

        if advance == 0 {
            return Err(DecodeFailed::UnexpectedEof);
        }
        block.remaining = block
            .remaining
            .checked_sub(advance as u32)
            .ok_or(DecodeFailed::OverreadBlock)?;
    }
    // A match may run past the end of a chunk only in corrupt data.
    if output.len() != output_len {
        return Err(DecodeFailed::OverreadBlock);
    }

    undo_e8_translation(&mut output);
    Ok(output)
}

/// Turns the absolute targets of x86 `CALL` instructions (E8) back into relative ones.
fn undo_e8_translation(data: &mut [u8]) {
    let mut pos = 0;
    // > E8 fixups are only performed for up to 10 bytes before the end of a chunk.
    while pos + 10 < data.len() {
        if data[pos] != 0xE8 {
            pos += 1;
            continue;
        }
        let current = pos as i32;
        let absolute =
            i32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]]);
        if absolute >= -current && absolute < E8_TRANSLATION_SIZE {
            // `lzxd` takes the second branch for 0 too, unlike the specification.
            let relative = if absolute >= 0 {
                absolute - current
            } else {
                absolute + E8_TRANSLATION_SIZE
            };
            data[pos + 1..pos + 5].copy_from_slice(&relative.to_le_bytes());
        }
        pos += 5;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e8_translation_is_relative_to_the_chunk() {
        // Target 100 at position 0 and target 0 at position 5, then padding.
        let mut data = vec![0xE8, 100, 0, 0, 0, 0xE8, 0, 0, 0, 0];
        data.extend([0; 11]);
        undo_e8_translation(&mut data);
        assert_eq!(&data[..5], &[0xE8, 100, 0, 0, 0]);
        assert_eq!(&data[5..10], &[0xE8, 0xFB, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn e8_translation_skips_the_last_ten_bytes() {
        let mut data = vec![0; 10];
        data.extend([0xE8, 100, 0, 0, 0, 0, 0, 0, 0, 0]);
        let before = data.clone();
        undo_e8_translation(&mut data);
        assert_eq!(data, before);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        for seed in 0..200u32 {
            let data: Vec<u8> = (0..300u32)
                .map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed * 97) >> 13) as u8)
                .collect();
            let _ = decompress_chunk(&data, CHUNK_SIZE);
        }
        assert_eq!(decompress_chunk(&[], 10), Err(DecodeFailed::UnexpectedEof));
        assert_eq!(
            decompress_chunk(&[], CHUNK_SIZE + 1),
            Err(DecodeFailed::ChunkTooLong)
        );
    }
}
