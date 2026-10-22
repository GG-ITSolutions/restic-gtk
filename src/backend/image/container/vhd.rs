//! VHD (Virtual PC, Hyper-V generation 1), fixed and dynamic.
//!
//! The footer at the end of the file describes the disk; a fixed disk is the raw
//! disk followed by that footer. A dynamic disk repeats the footer at the start and
//! stores the disk in blocks, found through the block allocation table (BAT); each
//! block begins with a bitmap of its written sectors, which only matters for
//! differencing disks, and those are refused. All fields are big-endian.

use std::io;

use super::{Extent, Files, Kind, MAX_TABLE, Mapping, Result, SECTOR, be32, be64, corrupt, refuse};

pub const COOKIE: &[u8] = b"conectix";
const HEADER_COOKIE: &[u8] = b"cxsparse";
const FOOTER_LEN: usize = 512;
const HEADER_LEN: usize = 1024;

const TYPE_FIXED: u32 = 2;
const TYPE_DYNAMIC: u32 = 3;
const TYPE_DIFFERENCING: u32 = 4;
const UNALLOCATED: u32 = u32::MAX;

pub enum Vhd {
    Fixed {
        size: u64,
    },
    Dynamic {
        size: u64,
        block_size: u64,
        /// The sector bitmap in front of every block's data, padded to sectors.
        bitmap_len: u64,
        /// Sector numbers of the blocks' bitmaps.
        bat: Vec<u32>,
    },
}

pub fn open(files: &mut dyn Files, len: u64) -> Result<Box<dyn Mapping>> {
    // A fixed disk has only the footer at the end; a dynamic one also a copy at the
    // start, which stands in for a damaged footer.
    let mut footer = [0; FOOTER_LEN];
    if len >= SECTOR {
        files.read_exact_at(0, len - SECTOR, &mut footer)?;
    }
    if !footer.starts_with(COOKIE) {
        files.read_exact_at(0, 0, &mut footer)?;
        if !footer.starts_with(COOKIE) {
            return Err(corrupt("VHD footer is missing").into());
        }
    }
    if be32(&footer, 12)? >> 16 != 1 {
        return refuse("VHD format version isn't 1");
    }
    let header_offset = be64(&footer, 16)?;
    let size = be64(&footer, 48)?;
    match be32(&footer, 60)? {
        TYPE_FIXED => {
            // The disk is followed by the footer; a shorter file would read it as data.
            if len < size.saturating_add(FOOTER_LEN as u64) {
                return Err(corrupt("fixed VHD is shorter than its disk").into());
            }
            Ok(Box::new(Vhd::Fixed { size }))
        }
        TYPE_DYNAMIC => {
            let mut header = [0; HEADER_LEN];
            files.read_exact_at(0, header_offset, &mut header)?;
            if !header.starts_with(HEADER_COOKIE) {
                return Err(corrupt("dynamic VHD header is missing").into());
            }
            let table_offset = be64(&header, 16)?;
            let max_entries = u64::from(be32(&header, 28)?);
            let block_size = u64::from(be32(&header, 32)?);
            if !block_size.is_power_of_two() || !(SECTOR..=256 << 20).contains(&block_size) {
                return refuse(format!("VHD block size {block_size} is out of range"));
            }
            let blocks = size.div_ceil(block_size);
            if max_entries < blocks {
                return refuse("VHD block table is smaller than the disk");
            }
            if blocks * 4 > MAX_TABLE as u64 {
                return refuse("VHD block table is too large");
            }
            let mut table = vec![0; (blocks * 4) as usize];
            files.read_exact_at(0, table_offset, &mut table)?;
            let bat = table
                .as_chunks::<4>()
                .0
                .iter()
                .map(|e| u32::from_be_bytes(*e))
                .collect();
            Ok(Box::new(Vhd::Dynamic {
                size,
                block_size,
                bitmap_len: (block_size / SECTOR).div_ceil(8).div_ceil(SECTOR) * SECTOR,
                bat,
            }))
        }
        TYPE_DIFFERENCING => refuse(
            "VHD differencing disk needs its parent image, which isn't read from other files",
        ),
        other => refuse(format!("VHD type {other} can't be browsed")),
    }
}

impl Mapping for Vhd {
    fn size(&self) -> u64 {
        match self {
            Self::Fixed { size } | Self::Dynamic { size, .. } => *size,
        }
    }

    fn map(&self, offset: u64, _: &mut dyn Files) -> io::Result<Extent> {
        match self {
            Self::Fixed { size } => Ok(Extent {
                len: size - offset,
                kind: Kind::Data { file: 0, offset },
            }),
            Self::Dynamic {
                size,
                block_size,
                bitmap_len,
                bat,
            } => {
                let within = offset % block_size;
                let len = (block_size - within).min(size - offset);
                let entry = *bat
                    .get((offset / block_size) as usize)
                    .ok_or_else(|| corrupt("VHD offset beyond the block table"))?;
                let kind = if entry == UNALLOCATED {
                    Kind::Zero
                } else {
                    Kind::Data {
                        file: 0,
                        offset: u64::from(entry) * SECTOR + bitmap_len + within,
                    }
                };
                Ok(Extent { len, kind })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{container, fixture, opened, read_all, refusal, try_disk};
    use super::*;

    fn footer(size: u64, kind: u32, header_offset: u64) -> Vec<u8> {
        let mut footer = vec![0; FOOTER_LEN];
        footer[..8].copy_from_slice(COOKIE);
        footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        footer[16..24].copy_from_slice(&header_offset.to_be_bytes());
        footer[48..56].copy_from_slice(&size.to_be_bytes());
        footer[60..64].copy_from_slice(&kind.to_be_bytes());
        footer
    }

    /// Footer copy, header at 512, BAT at 1536 with `entries`, blocks of 1024 bytes, footer.
    fn dynamic(size: u64, entries: &[u32], blocks: &[&[u8]]) -> Vec<u8> {
        let mut file = footer(size, TYPE_DYNAMIC, 512);
        let mut header = vec![0; HEADER_LEN];
        header[..8].copy_from_slice(HEADER_COOKIE);
        header[16..24].copy_from_slice(&1536u64.to_be_bytes());
        header[28..32].copy_from_slice(&(entries.len() as u32).to_be_bytes());
        header[32..36].copy_from_slice(&1024u32.to_be_bytes());
        file.extend(header);
        let mut bat: Vec<u8> = entries.iter().flat_map(|e| e.to_be_bytes()).collect();
        bat.resize(512, 0);
        file.extend(bat);
        for block in blocks {
            file.extend([0xff; 512]); // The sector bitmap.
            file.extend_from_slice(block);
        }
        file.extend(footer(size, TYPE_DYNAMIC, 512));
        file
    }

    #[test]
    fn reads_fixed_and_dynamic_disks() {
        let mut fixed: Vec<u8> = (0..200u8).collect();
        fixed.extend(footer(150, TYPE_FIXED, u64::MAX));
        let mut disk = opened(fixed);
        assert_eq!(disk.size(), 150);
        assert_eq!(read_all(&mut disk), (0..150u8).collect::<Vec<_>>());

        // Block 0 is the second block in the file, block 1 unallocated, block 2 the first.
        let data = dynamic(
            2500,
            &[(2048 + 1024 + 512) / 512, UNALLOCATED, 2048 / 512],
            &[&[2; 1024], &[1; 1024]],
        );
        let mut disk = opened(data.clone());
        let read = read_all(&mut disk);
        assert_eq!(read.len(), 2500);
        assert!(read[..1024].iter().all(|b| *b == 1));
        assert!(read[1024..2048].iter().all(|b| *b == 0));
        assert!(read[2048..].iter().all(|b| *b == 2));

        // Only the copy of the footer at the start survives.
        let mut truncated = data;
        truncated.truncate(truncated.len() - 512);
        assert_eq!(opened(truncated).size(), 2500);
    }

    #[test]
    fn refuses_other_kinds() {
        let mut fixed = vec![0; 100];
        fixed.extend(footer(100, TYPE_DIFFERENCING, u64::MAX));
        assert!(refusal(fixed).contains("parent"));
        let mut fixed = vec![0; 100];
        fixed.extend(footer(100, 99, u64::MAX));
        assert!(refusal(fixed).contains("type 99"));

        let mut fixed = vec![0; 100];
        let mut bad = footer(100, TYPE_FIXED, u64::MAX);
        bad[12..16].copy_from_slice(&0x0002_0000u32.to_be_bytes());
        fixed.extend(bad);
        assert!(refusal(fixed).contains("version"));

        // A short fixed disk, one missing a single byte in front of its footer, and a
        // BAT too small for the disk.
        let mut short = vec![0; 50];
        short.extend(footer(1000, TYPE_FIXED, u64::MAX));
        assert!(try_disk(short).is_err());
        let mut short = vec![0; 999];
        short.extend(footer(1000, TYPE_FIXED, u64::MAX));
        assert_eq!(short.len(), 1000 + 511);
        assert!(try_disk(short).is_err());
        assert!(refusal(dynamic(3000, &[UNALLOCATED], &[])).contains("block table"));
    }

    #[test]
    fn decodes_the_fixtures() {
        let ntfs = fixture("ntfs.img");
        for name in ["fixed.vhd", "dynamic.vhd"] {
            let mut disk = container(name);
            let data = read_all(&mut disk);
            // qemu rounds the size up to the CHS geometry, the rest is zeros.
            assert!(
                data.len() >= ntfs.len() && data[..ntfs.len()] == ntfs,
                "{name}"
            );
            assert!(data[ntfs.len()..].iter().all(|b| *b == 0), "{name}");
        }
    }
}
