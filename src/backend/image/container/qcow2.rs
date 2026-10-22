//! qcow2 (QEMU, Proxmox), versions 2 and 3.
//!
//! The virtual disk is split into clusters; the L1 table points to L2 tables, whose
//! entries say where a cluster is, that it is all zeros, or that it is stored
//! compressed. Only the active tables are read: internal snapshots don't matter, and
//! a backing file would be another image, so images with one are refused. All fields
//! are big-endian.

use std::io;

use super::{
    Codec, Extent, Files, Kind, MAX_TABLE, Mapping, Packed, Result, TableCache, be32, be64,
    corrupt, refuse,
};

pub const MAGIC: &[u8] = b"QFI\xfb";

/// The header up to and including `compression_type`.
const HEADER_LEN: usize = 105;
/// The header of version 2 ends after `l1_table_offset`.
const HEADER_LEN_V2: usize = 72;
/// Longest backing file name per the spec.
const MAX_BACKING_NAME: u32 = 1023;

/// Bits 9..55 of an L1 or L2 entry: the host offset of the table or cluster.
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
/// L2 entry bit 62: a compressed cluster.
const COMPRESSED: u64 = 1 << 62;
/// L2 entry bit 0: a cluster that reads as zeros, whatever its offset says.
const ZERO: u64 = 1;

const FEATURE_DIRTY: u64 = 1 << 0;
const FEATURE_CORRUPT: u64 = 1 << 1;
const FEATURE_EXTERNAL_DATA: u64 = 1 << 2;
const FEATURE_COMPRESSION_TYPE: u64 = 1 << 3;
const FEATURE_EXTENDED_L2: u64 = 1 << 4;

/// Memory for L2 tables: each maps `cluster_size / 8` clusters.
const L2_CACHE_BYTES: u64 = 4 << 20;

pub struct Qcow2 {
    size: u64,
    cluster_bits: u32,
    /// The entries that cover `size`.
    l1: Vec<u64>,
    compression: Codec,
    l2: TableCache,
}

pub fn open(files: &mut dyn Files) -> Result<Box<dyn Mapping>> {
    let mut header = [0; HEADER_LEN];
    let n = files.read_at(0, 0, &mut header)?;
    let header = &header[..n];
    let version = be32(header, 4)?;
    if !(2..=3).contains(&version) {
        return refuse(format!("qcow2 version {version} can't be browsed"));
    }
    if header.len()
        < if version == 2 {
            HEADER_LEN_V2
        } else {
            HEADER_LEN - 1
        }
    {
        return Err(corrupt("qcow2 header is truncated").into());
    }

    let backing_offset = be64(header, 8)?;
    if backing_offset != 0 {
        let len = be32(header, 16)?.min(MAX_BACKING_NAME);
        let mut name = vec![0; len as usize];
        let shown = match files.read_exact_at(0, backing_offset, &mut name) {
            Ok(()) => format!(" “{}”", String::from_utf8_lossy(&name)),
            Err(_) => String::new(),
        };
        return refuse(format!(
            "qcow2 image needs its backing file{shown}, which isn't read from other files"
        ));
    }
    let cluster_bits = be32(header, 20)?;
    if !(9..=21).contains(&cluster_bits) {
        return refuse(format!(
            "qcow2 cluster size 2^{cluster_bits} is out of range"
        ));
    }
    let size = be64(header, 24)?;
    if be32(header, 32)? != 0 {
        return refuse("qcow2 image is encrypted");
    }
    let l1_size = be32(header, 36)?;
    let l1_offset = be64(header, 40)?;

    let features = if version == 2 { 0 } else { be64(header, 72)? };
    let header_len = if version == 2 {
        HEADER_LEN_V2 as u32
    } else {
        be32(header, 100)?
    };
    let compression_field = if header_len > 104 {
        *header
            .get(104)
            .ok_or_else(|| corrupt("qcow2 header is truncated"))?
    } else {
        0
    };
    if features & FEATURE_CORRUPT != 0 {
        return refuse("qcow2 image is marked corrupt");
    }
    if features & FEATURE_EXTERNAL_DATA != 0 {
        return refuse("qcow2 image uses an external data file");
    }
    if features & FEATURE_EXTENDED_L2 != 0 {
        return refuse("qcow2 image uses extended L2 entries, which can't be browsed yet");
    }
    if features & !(FEATURE_DIRTY | FEATURE_COMPRESSION_TYPE) != 0 {
        return refuse("qcow2 image uses unknown features");
    }
    // The dirty bit only means stale refcounts, which aren't read.
    let compression = match (features & FEATURE_COMPRESSION_TYPE != 0, compression_field) {
        (false, _) | (true, 0) => Codec::Deflate,
        (true, 1) => Codec::Zstd,
        (true, other) => return refuse(format!("qcow2 compression type {other} can't be browsed")),
    };

    let cluster_size = 1u64 << cluster_bits;
    let l2_entries = cluster_size / 8;
    let needed = size.div_ceil(l2_entries << cluster_bits);
    if u64::from(l1_size) < needed {
        return refuse("qcow2 L1 table is smaller than the disk");
    }
    if needed * 8 > MAX_TABLE as u64 {
        return refuse("qcow2 L1 table is too large");
    }
    let mut table = vec![0; (needed * 8) as usize];
    files.read_exact_at(0, l1_offset, &mut table)?;
    let l1 = table
        .as_chunks::<8>()
        .0
        .iter()
        .map(|e| u64::from_be_bytes(*e))
        .collect();

    Ok(Box::new(Qcow2 {
        size,
        cluster_bits,
        l1,
        compression,
        l2: TableCache::new((L2_CACHE_BYTES / cluster_size) as usize),
    }))
}

/// Host offset and stored length of the compressed cluster described by an L2 entry.
///
/// The entry holds the offset in its low bits and, above them, the number of
/// additional sectors the data reaches into; the data ends at a sector boundary.
fn split_compressed(entry: u64, cluster_bits: u32) -> (u64, usize) {
    let offset_bits = 62 - (cluster_bits - 8);
    let offset = entry & ((1 << offset_bits) - 1);
    let sectors = (entry >> offset_bits) & ((1 << (cluster_bits - 8)) - 1);
    let stored = (sectors + 1) * 512 - offset % 512;
    (offset, stored as usize)
}

impl Mapping for Qcow2 {
    fn size(&self) -> u64 {
        self.size
    }

    fn map(&self, offset: u64, files: &mut dyn Files) -> io::Result<Extent> {
        let cluster_size = 1u64 << self.cluster_bits;
        let l2_bits = self.cluster_bits - 3;
        let within = offset & (cluster_size - 1);
        let run = cluster_size - within;
        let cluster = offset >> self.cluster_bits;
        let l1_index = (cluster >> l2_bits) as usize;
        let l2_index = (cluster & ((1 << l2_bits) - 1)) as usize;

        let l1_entry = *self
            .l1
            .get(l1_index)
            .ok_or_else(|| corrupt("qcow2 offset beyond the L1 table"))?;
        let l2_offset = l1_entry & OFFSET_MASK;
        if l2_offset == 0 {
            // Nothing in the whole range of this L2 table.
            let range = cluster_size << l2_bits;
            return Ok(Extent {
                len: range - offset % range,
                kind: Kind::Zero,
            });
        }
        if !l2_offset.is_multiple_of(cluster_size) {
            return Err(corrupt("qcow2 L2 table isn't cluster aligned"));
        }
        let table = self.l2.get(l2_offset, || {
            let mut table = vec![0; cluster_size as usize];
            files.read_exact_at(0, l2_offset, &mut table)?;
            Ok(table)
        })?;
        let entry = be64(&table, l2_index * 8)?;

        let kind = if entry & COMPRESSED != 0 {
            let (host, stored) = split_compressed(entry, self.cluster_bits);
            Kind::Packed(Packed {
                file: 0,
                offset: host,
                stored,
                codec: self.compression,
                start: offset - within,
                plain: cluster_size as usize,
            })
        } else if entry & ZERO != 0 || entry & OFFSET_MASK == 0 {
            Kind::Zero
        } else {
            let host = entry & OFFSET_MASK;
            if !host.is_multiple_of(cluster_size) {
                return Err(corrupt("qcow2 cluster isn't cluster aligned"));
            }
            Kind::Data {
                file: 0,
                offset: host + within,
            }
        };
        Ok(Extent { len: run, kind })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use super::super::tests::{container, fixture, opened, read_all, refusal};
    use super::*;

    #[test]
    fn splits_compressed_entries() {
        assert_eq!(
            split_compressed((2 << 54) | 0x1234_5678, 16),
            (0x1234_5678, 3 * 512 - 0x78)
        );
        // 512-byte clusters: one bit of sector count at bit 61.
        assert_eq!(split_compressed((1 << 61) | 1000, 9), (1000, 1024 - 488));
        assert_eq!(split_compressed(1000, 9), (1000, 512 - 488));
        // 4 KiB clusters: four bits at 58; 2 MiB: thirteen bits at 49.
        assert_eq!(split_compressed((15 << 58) | 512, 12), (512, 16 * 512));
        assert_eq!(
            split_compressed((0x1fff << 49) | ((1 << 49) - 1), 21),
            ((1 << 49) - 1, 0x2000 * 512 - 511)
        );
    }

    /// Cluster 0 header, 1 L1, 2 L2, 3.. data; 512-byte clusters, 64 entries per L2.
    struct Image {
        clusters: Vec<Vec<u8>>,
    }

    impl Image {
        fn new(version: u32, size: u64) -> Self {
            let mut header = vec![0; 512];
            header[..4].copy_from_slice(MAGIC);
            header[4..8].copy_from_slice(&version.to_be_bytes());
            header[20..24].copy_from_slice(&9u32.to_be_bytes());
            header[24..32].copy_from_slice(&size.to_be_bytes());
            header[36..40].copy_from_slice(&1u32.to_be_bytes());
            header[40..48].copy_from_slice(&512u64.to_be_bytes());
            header[100..104].copy_from_slice(&104u32.to_be_bytes());
            let mut l1 = vec![0; 512];
            l1[..8].copy_from_slice(&(1024u64 | 1 << 63).to_be_bytes());
            Self {
                clusters: vec![header, l1, vec![0; 512]],
            }
        }

        fn header(&mut self, at: usize, value: &[u8]) -> &mut Self {
            self.clusters[0][at..at + value.len()].copy_from_slice(value);
            self
        }

        fn l2(&mut self, index: usize, entry: u64) -> &mut Self {
            self.clusters[2][index * 8..index * 8 + 8].copy_from_slice(&entry.to_be_bytes());
            self
        }

        /// Appends a cluster and returns its offset.
        fn push(&mut self, data: &[u8]) -> u64 {
            let mut cluster = data.to_vec();
            cluster.resize(data.len().div_ceil(512) * 512, 0);
            self.clusters.push(cluster);
            ((self.clusters.len() - 1) * 512) as u64
        }

        fn bytes(&self) -> Vec<u8> {
            self.clusters.concat()
        }
    }

    #[test]
    fn maps_every_kind_of_cluster() {
        let mut image = Image::new(3, 6 * 512);
        let plain = image.push(&[1; 512]);
        let zero_with_data = image.push(&[2; 512]);
        let packed: Vec<u8> = (0..512u32).map(|i| (i % 5) as u8).collect();
        let mut compressed = miniz_oxide::deflate::compress_to_vec(&packed, 9);
        // Tail of a second sector, as QEMU would store it.
        compressed.resize(compressed.len().max(513), 0);
        let stored = image.push(&compressed);
        let sectors = compressed.len().div_ceil(512) as u64 - 1;
        image
            .l2(1, plain | 1 << 63)
            .l2(2, zero_with_data | ZERO)
            .l2(3, COMPRESSED | (sectors << 61) | stored)
            // Cluster 4 unallocated, cluster 5 beyond the pushed clusters but zero.
            .l2(5, ZERO);
        let mut disk = opened(image.bytes());
        assert_eq!(disk.size(), 6 * 512);
        let data = read_all(&mut disk);
        assert_eq!(&data[..512], &[0; 512]);
        assert_eq!(&data[512..1024], &[1; 512]);
        assert_eq!(&data[1024..1536], &[0; 512]);
        assert_eq!(&data[1536..2048], &packed[..]);
        assert_eq!(&data[2048..], &[0; 1024]);

        disk.seek(SeekFrom::Start(1536 + 100)).unwrap();
        let mut buf = [0; 10];
        disk.read_exact(&mut buf).unwrap();
        assert_eq!(buf, packed[100..110]);

        // An L1 entry without a table: zeros for its whole range.
        let mut image = Image::new(3, 6 * 512);
        image.clusters[1][..8].fill(0);
        assert_eq!(read_all(&mut opened(image.bytes())), vec![0; 6 * 512]);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let refused = |edit: &dyn Fn(&mut Image)| {
            let mut image = Image::new(3, 1024);
            edit(&mut image);
            refusal(image.bytes())
        };
        let accepted = |edit: &dyn Fn(&mut Image)| {
            let mut image = Image::new(3, 1024);
            edit(&mut image);
            opened(image.bytes()).size()
        };
        let reason = refused(&|i| {
            i.header(8, &1536u64.to_be_bytes())
                .header(16, &4u32.to_be_bytes());
            i.push(b"base.img");
        });
        assert!(
            reason.contains("backing file") && reason.contains("“base”"),
            "{reason}"
        );
        assert!(
            refused(&|i| {
                i.header(32, &1u32.to_be_bytes());
            })
            .contains("encrypted")
        );
        assert!(
            refused(&|i| {
                i.header(72, &FEATURE_CORRUPT.to_be_bytes());
            })
            .contains("corrupt")
        );
        assert!(
            refused(&|i| {
                i.header(72, &FEATURE_EXTERNAL_DATA.to_be_bytes());
            })
            .contains("external")
        );
        assert!(
            refused(&|i| {
                i.header(72, &FEATURE_EXTENDED_L2.to_be_bytes());
            })
            .contains("extended L2")
        );
        assert!(
            refused(&|i| {
                i.header(72, &(1u64 << 5).to_be_bytes());
            })
            .contains("unknown features")
        );
        assert!(
            refused(&|i| {
                i.header(4, &1u32.to_be_bytes());
            })
            .contains("version 1")
        );
        assert!(
            refused(&|i| {
                i.header(20, &8u32.to_be_bytes());
            })
            .contains("cluster size")
        );
        assert!(
            refused(&|i| {
                i.header(20, &22u32.to_be_bytes());
            })
            .contains("cluster size")
        );
        assert!(
            refused(&|i| {
                i.header(36, &0u32.to_be_bytes());
            })
            .contains("L1 table")
        );
        assert!(
            refused(&|i| {
                i.header(72, &FEATURE_COMPRESSION_TYPE.to_be_bytes())
                    .header(100, &105u32.to_be_bytes())
                    .header(104, &[7]);
            })
            .contains("compression type 7")
        );

        assert_eq!(
            accepted(&|i| {
                i.header(72, &FEATURE_DIRTY.to_be_bytes());
            }),
            1024
        );
        assert_eq!(
            accepted(&|i| {
                i.header(4, &2u32.to_be_bytes());
            }),
            1024
        );
        assert_eq!(
            accepted(&|i| {
                i.header(72, &FEATURE_COMPRESSION_TYPE.to_be_bytes())
                    .header(100, &105u32.to_be_bytes())
                    .header(104, &[1]);
            }),
            1024
        );
        // A version 2 image that is only 72 bytes long.
        let mut image = Image::new(2, 0).bytes();
        image.truncate(72);
        assert_eq!(opened(image).size(), 0);
    }

    #[test]
    fn decodes_the_fixtures() {
        let ntfs = fixture("ntfs.img");
        for name in ["plain.qcow2", "zlib.qcow2", "zstd.qcow2"] {
            let mut disk = container(name);
            assert_eq!(disk.size(), ntfs.len() as u64, "{name}");
            assert!(read_all(&mut disk) == ntfs, "{name} differs");
        }
        assert!(refusal(fixture("containers/backing.qcow2")).contains("backing file “ntfs.img”"));
    }
}
