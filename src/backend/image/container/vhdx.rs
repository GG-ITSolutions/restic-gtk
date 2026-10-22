//! VHDX (Hyper-V, Windows Backup), fixed and dynamic.
//!
//! Two headers (the one with the higher sequence number and a valid checksum is
//! current) lead to a region table, which places the block allocation table (BAT)
//! and the metadata (block size, disk size, sector size, flags). The BAT maps
//! payload blocks to file offsets; after every chunk of payload entries comes one
//! for a sector bitmap, which only differencing disks use, and those are refused.
//! So is a disk with a log that would have to be replayed. All fields are little-endian.

use std::io;

use super::{Extent, Files, Kind, MAX_TABLE, Mapping, Result, corrupt, le16, le32, le64, refuse};

pub const MAGIC: &[u8] = b"vhdxfile";
const HEADER_SIGNATURE: &[u8] = b"head";
const REGION_SIGNATURE: &[u8] = b"regi";
const METADATA_SIGNATURE: &[u8] = b"metadata";

const HEADER_OFFSETS: [u64; 2] = [0x1_0000, 0x2_0000];
const HEADER_LEN: usize = 4096;
const REGION_TABLE_OFFSETS: [u64; 2] = [0x3_0000, 0x4_0000];
/// The region table and the metadata table are each at most 64 KiB.
const TABLE_LEN: usize = 0x1_0000;
const MAX_ENTRIES: usize = 2047;

/// Region GUIDs, as the bytes lie in the file.
const REGION_BAT: [u8; 16] = [
    0x66, 0x77, 0xc2, 0x2d, 0x23, 0xf6, 0x00, 0x42, 0x9d, 0x64, 0x11, 0x5e, 0x9b, 0xfd, 0x4a, 0x08,
];
const REGION_METADATA: [u8; 16] = [
    0x06, 0xa2, 0x7c, 0x8b, 0x90, 0x47, 0x9a, 0x4b, 0xb8, 0xfe, 0x57, 0x5f, 0x05, 0x0f, 0x88, 0x6e,
];
/// Metadata item GUIDs.
const ITEM_FILE_PARAMETERS: [u8; 16] = [
    0x37, 0x67, 0xa1, 0xca, 0x36, 0xfa, 0x43, 0x4d, 0xb3, 0xb6, 0x33, 0xf0, 0xaa, 0x44, 0xe7, 0x6b,
];
const ITEM_DISK_SIZE: [u8; 16] = [
    0x24, 0x42, 0xa5, 0x2f, 0x1b, 0xcd, 0x76, 0x48, 0xb2, 0x11, 0x5d, 0xbe, 0xd8, 0x3b, 0xf4, 0xb8,
];
const ITEM_SECTOR_SIZE: [u8; 16] = [
    0x1d, 0xbf, 0x41, 0x81, 0x6f, 0xa9, 0x09, 0x47, 0xba, 0x47, 0xf2, 0x33, 0xa8, 0xfa, 0xab, 0x5f,
];
/// Items that are flagged required but don't matter for reading: the physical
/// sector size, the disk's identity (page 83 data) and the parent locator, whose
/// presence the file parameters announce.
const ITEMS_IGNORED: [[u8; 16]; 3] = [
    [
        0xc7, 0x48, 0xa3, 0xcd, 0x5d, 0x44, 0x71, 0x44, 0x9c, 0xc9, 0xe9, 0x88, 0x52, 0x51, 0xc5,
        0x56,
    ],
    [
        0xab, 0x12, 0xca, 0xbe, 0xe6, 0xb2, 0x23, 0x45, 0x93, 0xef, 0xc3, 0x09, 0xe0, 0x00, 0xc7,
        0x46,
    ],
    [
        0x2d, 0x5f, 0xd3, 0xa8, 0x0b, 0xb3, 0x4d, 0x45, 0xab, 0xf7, 0xd3, 0xd8, 0x48, 0x34, 0xab,
        0x0c,
    ],
];
const ITEM_REQUIRED: u32 = 1 << 2;
const PARAMETERS_HAS_PARENT: u32 = 1 << 1;

const STATE_MASK: u64 = 7;
const STATE_UNMAPPED: u64 = 3;
const STATE_FULLY_PRESENT: u64 = 6;
/// BAT entries hold the file offset in units of 1 MiB from bit 20 on.
const OFFSET_SHIFT: u32 = 20;

pub struct Vhdx {
    size: u64,
    block_size: u64,
    chunk_ratio: u64,
    bat: Vec<u64>,
}

pub fn open(files: &mut dyn Files) -> Result<Box<dyn Mapping>> {
    let mut header = [0; HEADER_LEN];
    let mut current: Option<(u64, [u8; HEADER_LEN])> = None;
    for offset in HEADER_OFFSETS {
        files.read_exact_at(0, offset, &mut header)?;
        let mut unsigned = header;
        unsigned[4..8].fill(0);
        if !header.starts_with(HEADER_SIGNATURE)
            || crc32c(&unsigned) != le32(&header, 4)?
            || le16(&header, 66)? != 1
        {
            continue;
        }
        let sequence = le64(&header, 8)?;
        if current.is_none_or(|(s, _)| sequence > s) {
            current = Some((sequence, header));
        }
    }
    let Some((_, header)) = current else {
        return Err(corrupt("VHDX has no valid header").into());
    };
    if header[48..64].iter().any(|b| *b != 0) {
        return refuse("VHDX image has a log that can't be replayed yet");
    }

    let mut regions = vec![0; TABLE_LEN];
    for offset in REGION_TABLE_OFFSETS {
        files.read_exact_at(0, offset, &mut regions)?;
        if regions.starts_with(REGION_SIGNATURE) {
            break;
        }
    }
    if !regions.starts_with(REGION_SIGNATURE) {
        return Err(corrupt("VHDX has no region table").into());
    }
    let (mut bat, mut metadata) = (None, None);
    for i in 0..(le32(&regions, 8)? as usize).min(MAX_ENTRIES) {
        let entry = &regions[16 + 32 * i..48 + 32 * i];
        let place = (le64(entry, 16)?, le32(entry, 24)?);
        match entry[..16].try_into() {
            Ok(REGION_BAT) => bat = Some(place),
            Ok(REGION_METADATA) => metadata = Some(place),
            _ if le32(entry, 28)? != 0 => {
                return refuse("VHDX image has an unknown required region");
            }
            _ => {}
        }
    }
    let (Some((bat_offset, bat_len)), Some((metadata_offset, metadata_len))) = (bat, metadata)
    else {
        return Err(corrupt("VHDX region table lacks the BAT or the metadata").into());
    };

    let mut table = vec![0; TABLE_LEN.min(metadata_len as usize)];
    files.read_exact_at(0, metadata_offset, &mut table)?;
    if !table.starts_with(METADATA_SIGNATURE) {
        return Err(corrupt("VHDX metadata table is missing").into());
    }
    let (mut block_size, mut size, mut sector_size) = (None, None, None);
    for i in 0..(le16(&table, 10)? as usize).min(MAX_ENTRIES) {
        let entry = table
            .get(32 + 32 * i..64 + 32 * i)
            .ok_or_else(|| corrupt("VHDX metadata table is truncated"))?;
        let offset = metadata_offset
            .checked_add(u64::from(le32(entry, 16)?))
            .ok_or_else(|| corrupt("VHDX metadata item lies out of range"))?;
        let mut item = [0; 8];
        match entry[..16].try_into() {
            Ok(ITEM_FILE_PARAMETERS) => {
                files.read_exact_at(0, offset, &mut item)?;
                if le32(&item, 4)? & PARAMETERS_HAS_PARENT != 0 {
                    return refuse(
                        "VHDX differencing disk needs its parent image, which isn't read from other files",
                    );
                }
                block_size = Some(u64::from(le32(&item, 0)?));
            }
            Ok(ITEM_DISK_SIZE) => {
                files.read_exact_at(0, offset, &mut item)?;
                size = Some(le64(&item, 0)?);
            }
            Ok(ITEM_SECTOR_SIZE) => {
                files.read_exact_at(0, offset, &mut item[..4])?;
                sector_size = Some(u64::from(le32(&item, 0)?));
            }
            Ok(known) if ITEMS_IGNORED.contains(&known) => {}
            _ if le32(entry, 24)? & ITEM_REQUIRED != 0 => {
                return refuse("VHDX image has an unknown required metadata item");
            }
            _ => {}
        }
    }
    let (Some(block_size), Some(size), Some(sector_size)) = (block_size, size, sector_size) else {
        return Err(corrupt("VHDX metadata lacks the block size, disk size or sector size").into());
    };
    if !block_size.is_power_of_two() || !(1 << 20..=256 << 20).contains(&block_size) {
        return refuse(format!("VHDX block size {block_size} is out of range"));
    }
    if sector_size != 512 && sector_size != 4096 {
        return refuse(format!("VHDX sector size {sector_size} is out of range"));
    }

    let chunk_ratio = chunk_ratio(sector_size, block_size);
    let blocks = size.div_ceil(block_size);
    let entries = if blocks == 0 {
        0
    } else {
        bat_index(blocks - 1, chunk_ratio) + 1
    };
    if entries * 8 > MAX_TABLE as u64 {
        return refuse("VHDX block table is too large");
    }
    if entries * 8 > u64::from(bat_len) {
        return refuse("VHDX block table is smaller than the disk");
    }
    let mut table = vec![0; (entries * 8) as usize];
    files.read_exact_at(0, bat_offset, &mut table)?;
    let bat = table
        .as_chunks::<8>()
        .0
        .iter()
        .map(|e| u64::from_le_bytes(*e))
        .collect();
    Ok(Box::new(Vhdx {
        size,
        block_size,
        chunk_ratio,
        bat,
    }))
}

/// Payload blocks per sector bitmap block: a bitmap of one block covers 2^23 sectors.
fn chunk_ratio(sector_size: u64, block_size: u64) -> u64 {
    ((1 << 23) * sector_size / block_size).max(1)
}

/// The BAT entry of payload block `block`: one bitmap entry follows every `chunk_ratio` of them.
fn bat_index(block: u64, chunk_ratio: u64) -> u64 {
    block + block / chunk_ratio
}

/// CRC-32C (Castagnoli), bit by bit: 4 KiB per header, speed doesn't matter.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

impl Mapping for Vhdx {
    fn size(&self) -> u64 {
        self.size
    }

    fn map(&self, offset: u64, _: &mut dyn Files) -> io::Result<Extent> {
        let within = offset % self.block_size;
        let len = (self.block_size - within).min(self.size - offset);
        let index = bat_index(offset / self.block_size, self.chunk_ratio);
        let entry = *self
            .bat
            .get(index as usize)
            .ok_or_else(|| corrupt("VHDX offset beyond the block table"))?;
        let kind = match entry & STATE_MASK {
            0..=STATE_UNMAPPED => Kind::Zero,
            STATE_FULLY_PRESENT => {
                let block = (entry >> OFFSET_SHIFT) << OFFSET_SHIFT;
                if block == 0 {
                    return Err(corrupt("VHDX block lies in the file header"));
                }
                Kind::Data {
                    file: 0,
                    offset: block
                        .checked_add(within)
                        .ok_or_else(|| corrupt("VHDX block lies out of range"))?,
                }
            }
            state => return Err(corrupt(format!("VHDX block in state {state}"))),
        };
        Ok(Extent { len, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{container, fixture, opened, read_all, refusal};
    use super::*;

    #[test]
    fn computes_checksums_and_indexes() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(chunk_ratio(512, 32 << 20), 128);
        assert_eq!(chunk_ratio(4096, 32 << 20), 1024);
        assert_eq!(chunk_ratio(512, 1 << 20), 4096);
        assert_eq!(chunk_ratio(512, 8 << 20), 512);
        assert_eq!(
            (0..9).map(|b| bat_index(b, 4)).collect::<Vec<_>>(),
            [0, 1, 2, 3, 5, 6, 7, 8, 10]
        );
    }

    /// Re-signs the header at `offset` after an edit.
    fn sign(file: &mut [u8], offset: usize) {
        file[offset + 4..offset + 8].fill(0);
        let crc = crc32c(&file[offset..offset + HEADER_LEN]);
        file[offset + 4..offset + 8].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn chooses_the_header() {
        let ntfs = fixture("ntfs.img");
        let file = fixture("containers/dynamic.vhdx");
        let [first, second] = HEADER_OFFSETS.map(|o| o as usize);

        // qemu leaves the second header current; a damaged first one changes nothing.
        let mut damaged = file.clone();
        damaged[first + 100] ^= 1;
        assert_eq!(read_all(&mut opened(damaged))[..ntfs.len()], ntfs);

        // The first header wins with a higher sequence number, and its log GUID counts.
        let mut flipped = file.clone();
        let sequence = le64(&flipped, first + 8).unwrap() + 2;
        flipped[first + 8..first + 16].copy_from_slice(&sequence.to_le_bytes());
        flipped[first + 48..first + 64].fill(1);
        sign(&mut flipped, first);
        assert!(refusal(flipped.clone()).contains("log"));
        flipped[second + 48..second + 64].fill(1);
        sign(&mut flipped, second);
        flipped[first + 48..first + 64].fill(0);
        sign(&mut flipped, first);
        assert_eq!(read_all(&mut opened(flipped))[..ntfs.len()], ntfs);

        let mut broken = file.clone();
        broken[first + 100] ^= 1;
        broken[second + 100] ^= 1;
        assert!(super::super::tests::try_disk(broken).is_err());

        // The file parameters item: flags at metadata offset 0x10000 + 4.
        let mut child = file;
        child[0x31_0004] |= PARAMETERS_HAS_PARENT as u8;
        assert!(refusal(child).contains("parent"));
    }

    #[test]
    fn maps_block_states() {
        let vhdx = Vhdx {
            size: 3 << 20,
            block_size: 1 << 20,
            chunk_ratio: 2,
            bat: vec![
                STATE_FULLY_PRESENT | 5 << OFFSET_SHIFT,
                STATE_UNMAPPED | 6 << OFFSET_SHIFT,
                0, // The bitmap entry of the first chunk.
                7,
            ],
        };
        let mut files = super::super::Sources::<std::io::Cursor<Vec<u8>>>(Vec::new());
        let extent = vhdx.map(100, &mut files).unwrap();
        assert!(matches!(extent.kind, Kind::Data { file: 0, offset } if offset == (5 << 20) + 100));
        assert_eq!(extent.len, (1 << 20) - 100);
        assert!(matches!(
            vhdx.map(1 << 20, &mut files).unwrap().kind,
            Kind::Zero
        ));
        assert!(matches!(
            vhdx.map(2 << 20, &mut files),
            Err(err) if err.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn decodes_the_fixtures() {
        let ntfs = fixture("ntfs.img");
        for name in ["fixed.vhdx", "dynamic.vhdx"] {
            let mut disk = container(name);
            assert_eq!(disk.size(), ntfs.len() as u64, "{name}");
            assert!(read_all(&mut disk) == ntfs, "{name} differs");
        }
    }
}
