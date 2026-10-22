//! VMDK (VMware): sparse extents, stream-optimized files and descriptors with extent files.
//!
//! A descriptor is a short text that lists the extents of the disk in order: flat
//! files (raw data), sparse files or zeros. A monolithic sparse file embeds its
//! descriptor and is its own single extent. A sparse extent keeps the disk in grains
//! that a grain directory and grain tables place in the file; a stream-optimized
//! file stores them deflated behind a marker and may put its directory at the end.
//! Disks with a parent (snapshots, `parentCID`) are refused. All fields are
//! little-endian, offsets in sectors of 512 bytes unless noted.

use std::io::{self, Read, Seek};

use super::{
    Codec, Error, Extent, Files, Kind, MAX_TABLE, Mapping, Packed, Result, SECTOR, Sources,
    TableCache, corrupt, le16, le32, le64, refuse,
};

pub const MAGIC: &[u8] = b"KDMV";
pub const MAGIC_COWD: &[u8] = b"COWD";
pub const DESCRIPTOR_MAGIC: &[u8] = b"# Disk DescriptorFile";

const HEADER_LEN: usize = 512;
const MAX_DESCRIPTOR: u64 = 1 << 20;
/// Grain size in sectors: at least 4 KiB, at most 32 MiB.
const GRAIN_SECTORS: std::ops::RangeInclusive<u64> = 8..=1 << 16;
/// Entries per grain table: a table is at most 16 KiB.
const TABLE_ENTRIES: std::ops::RangeInclusive<u64> = 1..=4096;
const FLAG_COMPRESSED: u32 = 1 << 16;
const COMPRESS_DEFLATE: u16 = 1;
/// A grain directory offset meaning "in the footer", 1024 bytes before the end.
const GD_AT_END: u64 = u64::MAX;
const FOOTER_FROM_END: u64 = 1024;
/// A compressed grain starts with its LBA (u64) and byte count (u32).
const MARKER_LEN: u64 = 12;
/// A grain table or directory entry of 0 is unallocated, 1 is a zeroed grain.
const ZEROED: u32 = 1;
const NO_PARENT: &str = "ffffffff";
const GRAIN_TABLES_CACHED: usize = 64;
/// The first word of an extent line.
const ACCESS_WORDS: [&str; 3] = ["RW", "RDONLY", "NOACCESS"];

const CREATE_TYPES: [&str; 10] = [
    "monolithicSparse",
    "streamOptimized",
    "monolithicFlat",
    "twoGbMaxExtentFlat",
    "twoGbMaxExtentSparse",
    "2GbMaxExtentFlat",
    "2GbMaxExtentSparse",
    "vmfs",
    "vmfsPreallocated",
    "vmfsEagerZeroedThick",
];

/// The disk: its extents in order.
pub struct Vmdk {
    size: u64,
    parts: Vec<Part>,
}

struct Part {
    start: u64,
    len: u64,
    kind: PartKind,
}

enum PartKind {
    Zero,
    /// Raw data from `offset` (bytes) in file `file`.
    Flat {
        file: usize,
        offset: u64,
    },
    Sparse(Sparse),
}

/// A sparse extent file: grain directory in memory, grain tables on demand.
struct Sparse {
    file: usize,
    /// Bytes.
    capacity: u64,
    /// Bytes.
    grain: u64,
    entries_per_table: u64,
    compressed: bool,
    /// Sector numbers of the grain tables.
    directory: Vec<u32>,
    tables: TableCache,
}

/// The fields of a sparse extent header.
struct Header {
    flags: u32,
    capacity: u64,
    grain: u64,
    descriptor: (u64, u64),
    entries_per_table: u64,
    directory: u64,
    algorithm: u16,
}

/// What a descriptor says, before it is checked.
#[derive(Debug, Default)]
struct Descriptor {
    create_type: Option<String>,
    parent_cid: Option<String>,
    parent_hint: Option<String>,
    extents: Vec<Line>,
}

#[derive(Debug)]
struct Line {
    sectors: u64,
    kind: LineKind,
    name: Option<String>,
    /// Sectors into a flat file.
    offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Flat,
    Sparse,
    Zero,
}

/// A monolithic sparse or stream-optimized file: one extent with an embedded descriptor.
pub fn open_sparse(files: &mut dyn Files) -> Result<Box<dyn Mapping>> {
    let (sparse, text) = Sparse::open(files, 0)?;
    if let Some(text) = text {
        descriptor(&text)?;
    }
    Ok(Box::new(Vmdk {
        size: sparse.capacity,
        parts: vec![Part {
            start: 0,
            len: sparse.capacity,
            kind: PartKind::Sparse(sparse),
        }],
    }))
}

/// A descriptor file of `len` bytes; its extents are opened with `sibling`.
pub fn open_descriptor<R: Read + Seek>(
    files: &mut Sources<R>,
    len: u64,
    sibling: &dyn Fn(&str) -> Result<Option<R>>,
) -> Result<Box<dyn Mapping>> {
    if len > MAX_DESCRIPTOR {
        return refuse("VMDK descriptor is larger than 1 MiB");
    }
    let mut text = vec![0; len as usize];
    files.read_exact_at(0, 0, &mut text)?;
    let parsed = descriptor(&text_of(&text))?;
    if parsed.extents.is_empty() {
        return refuse("VMDK descriptor names no extents");
    }

    let mut parts = Vec::new();
    let mut start = 0u64;
    for line in parsed.extents {
        let len = line
            .sectors
            .checked_mul(SECTOR)
            .filter(|len| start.checked_add(*len).is_some())
            .ok_or_else(|| Error::Image("VMDK extent sizes overflow".into()))?;
        if len == 0 {
            continue;
        }
        let kind = match line.kind {
            LineKind::Zero => PartKind::Zero,
            LineKind::Flat | LineKind::Sparse => {
                let name = line.name.unwrap_or_default();
                let Some(file) = sibling(&name)? else {
                    return refuse(format!("VMDK extent file “{name}” is not in the snapshot"));
                };
                files.0.push(file);
                let file = files.0.len() - 1;
                if line.kind == LineKind::Flat {
                    PartKind::Flat {
                        file,
                        offset: byte_offset(line.offset)?,
                    }
                } else {
                    let (sparse, _) = Sparse::open(files, file)?;
                    if sparse.capacity < len {
                        return refuse(format!(
                            "VMDK extent “{name}” is smaller than its descriptor says"
                        ));
                    }
                    PartKind::Sparse(sparse)
                }
            }
        };
        parts.push(Part { start, len, kind });
        start += len;
    }
    Ok(Box::new(Vmdk { size: start, parts }))
}

/// The byte offset of a sector number from a header, which may be anything.
fn byte_offset(sectors: u64) -> io::Result<u64> {
    sectors
        .checked_mul(SECTOR)
        .ok_or_else(|| corrupt("VMDK sector number is out of range"))
}

/// The text of a NUL-padded descriptor.
fn text_of(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Parses a descriptor and refuses what can't be browsed: parents, unknown disk
/// and extent types, extent names with a path.
fn descriptor(text: &str) -> Result<Descriptor> {
    let mut parsed = Descriptor::default();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // An extent line starts with its access word; only then may its name hold `=`.
        let first = line.split_whitespace().next().unwrap_or_default();
        let is_extent = ACCESS_WORDS.iter().any(|w| w.eq_ignore_ascii_case(first));
        if !is_extent && let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').to_owned();
            match key.trim().to_ascii_lowercase().as_str() {
                "createtype" => parsed.create_type = Some(value),
                "parentcid" => parsed.parent_cid = Some(value),
                "parentfilenamehint" => parsed.parent_hint = Some(value),
                _ => {}
            }
        } else {
            parsed.extents.push(extent_line(line)?);
        }
    }
    if let Some(cid) = &parsed.parent_cid
        && !cid.eq_ignore_ascii_case(NO_PARENT)
    {
        let hint = parsed
            .parent_hint
            .as_ref()
            .map(|hint| format!(" “{hint}”"))
            .unwrap_or_default();
        return refuse(format!(
            "VMDK snapshot needs its parent disk{hint}, which isn't read from other files"
        ));
    }
    if let Some(kind) = &parsed.create_type
        && !CREATE_TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind))
    {
        return refuse(format!("VMDK type “{kind}” can't be browsed"));
    }
    Ok(parsed)
}

/// `RW 8192 FLAT "disk-flat.vmdk" 0`: access, sectors, type, then a quoted name and an offset.
fn extent_line(line: &str) -> Result<Line> {
    let (head, rest) = line.split_at(line.find('"').unwrap_or(line.len()));
    let mut words = head.split_whitespace();
    let (Some(_access), Some(sectors), Some(kind)) = (words.next(), words.next(), words.next())
    else {
        return refuse(format!("VMDK descriptor line “{line}” isn't understood"));
    };
    let sectors = sectors
        .parse()
        .map_err(|_| Error::Image(format!("VMDK descriptor line “{line}” isn't understood")))?;
    let kind = match kind.to_ascii_uppercase().as_str() {
        "FLAT" | "VMFS" => LineKind::Flat,
        "SPARSE" => LineKind::Sparse,
        "ZERO" => LineKind::Zero,
        other => return refuse(format!("VMDK extent type “{other}” can't be browsed")),
    };
    let (name, offset) = match rest.rfind('"') {
        Some(end) if end > 0 => {
            let offset = rest[end + 1..].trim();
            let offset = if offset.is_empty() {
                0
            } else {
                offset.parse().map_err(|_| {
                    Error::Image(format!("VMDK descriptor line “{line}” isn't understood"))
                })?
            };
            (Some(rest[1..end].to_owned()), offset)
        }
        _ => (None, 0),
    };
    if let Some(name) = &name
        && name.contains(['/', '\\'])
    {
        return refuse(format!("VMDK extent name “{name}” contains a path"));
    }
    if name.is_none() && kind != LineKind::Zero {
        return refuse(format!("VMDK descriptor line “{line}” names no file"));
    }
    Ok(Line {
        sectors,
        kind,
        name,
        offset,
    })
}

impl Header {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        Ok(Self {
            flags: le32(bytes, 8)?,
            capacity: le64(bytes, 12)?,
            grain: le64(bytes, 20)?,
            descriptor: (le64(bytes, 28)?, le64(bytes, 36)?),
            entries_per_table: u64::from(le32(bytes, 44)?),
            directory: le64(bytes, 56)?,
            algorithm: le16(bytes, 77)?,
        })
    }
}

impl Sparse {
    /// Opens the sparse extent in `file`, returning it with its embedded descriptor.
    fn open(files: &mut dyn Files, file: usize) -> Result<(Self, Option<String>)> {
        let mut bytes = [0; HEADER_LEN];
        files.read_exact_at(file, 0, &mut bytes)?;
        if !bytes.starts_with(MAGIC) {
            return Err(corrupt("VMDK sparse extent lacks its magic").into());
        }
        let version = le32(&bytes, 4)?;
        if !(1..=3).contains(&version) {
            return refuse(format!(
                "VMDK sparse extent version {version} can't be browsed"
            ));
        }
        let mut header = Header::parse(&bytes)?;
        if header.directory == GD_AT_END {
            // Stream-optimized files written in one pass: the footer has the real header.
            let len = files.len(file)?;
            let footer = len
                .checked_sub(FOOTER_FROM_END)
                .ok_or_else(|| corrupt("VMDK stream is too short for its footer"))?;
            files.read_exact_at(file, footer, &mut bytes)?;
            if !bytes.starts_with(MAGIC) {
                return Err(corrupt("VMDK stream footer lacks its magic").into());
            }
            header = Header::parse(&bytes)?;
            if header.directory == GD_AT_END {
                return Err(corrupt("VMDK stream footer has no grain directory").into());
            }
        }

        let compressed = header.flags & FLAG_COMPRESSED != 0;
        if compressed && header.algorithm != COMPRESS_DEFLATE {
            return refuse(format!(
                "VMDK compression algorithm {} can't be browsed",
                header.algorithm
            ));
        }
        if !header.grain.is_power_of_two() || !GRAIN_SECTORS.contains(&header.grain) {
            return refuse(format!("VMDK grain size {} is out of range", header.grain));
        }
        if !TABLE_ENTRIES.contains(&header.entries_per_table) {
            return refuse("VMDK grain table size is out of range");
        }
        if header.capacity == 0 {
            return refuse("VMDK extent is empty");
        }
        let tables = header
            .capacity
            .div_ceil(header.entries_per_table * header.grain);
        if tables * 4 > MAX_TABLE as u64 {
            return refuse("VMDK grain directory is too large");
        }
        let mut table = vec![0; (tables * 4) as usize];
        files.read_exact_at(file, byte_offset(header.directory)?, &mut table)?;
        let directory = table
            .as_chunks::<4>()
            .0
            .iter()
            .map(|e| u32::from_le_bytes(*e))
            .collect();

        let (offset, sectors) = header.descriptor;
        let text = if sectors == 0 {
            None
        } else {
            let mut text = vec![0; (sectors.min(MAX_DESCRIPTOR / SECTOR) * SECTOR) as usize];
            let n = files.read_at(file, byte_offset(offset)?, &mut text)?;
            Some(text_of(&text[..n]))
        };
        Ok((
            Self {
                file,
                capacity: header.capacity * SECTOR,
                grain: header.grain * SECTOR,
                entries_per_table: header.entries_per_table,
                compressed,
                directory,
                tables: TableCache::new(GRAIN_TABLES_CACHED),
            },
            text,
        ))
    }

    /// The run at `offset` into this extent; a packed grain's `start` is relative to the extent.
    fn map(&self, offset: u64, files: &mut dyn Files) -> io::Result<Extent> {
        let within = offset % self.grain;
        let len = (self.grain - within).min(self.capacity - offset);
        let grain = offset / self.grain;
        let zero = Ok(Extent {
            len,
            kind: Kind::Zero,
        });

        let table = *self
            .directory
            .get((grain / self.entries_per_table) as usize)
            .ok_or_else(|| corrupt("VMDK grain beyond the grain directory"))?;
        if table <= ZEROED {
            return zero;
        }
        let entries = self.tables.get(u64::from(table), || {
            let mut entries = vec![0; (self.entries_per_table * 4) as usize];
            files.read_exact_at(self.file, u64::from(table) * SECTOR, &mut entries)?;
            Ok(entries)
        })?;
        let entry = le32(&entries, (grain % self.entries_per_table) as usize * 4)?;
        if entry <= ZEROED {
            return zero;
        }
        let at = u64::from(entry) * SECTOR;
        if !self.compressed {
            return Ok(Extent {
                len,
                kind: Kind::Data {
                    file: self.file,
                    offset: at + within,
                },
            });
        }

        let mut marker = [0; MARKER_LEN as usize];
        files.read_exact_at(self.file, at, &mut marker)?;
        if le64(&marker, 0)? != grain * (self.grain / SECTOR) {
            return Err(corrupt("VMDK grain marker names another grain"));
        }
        let stored = u64::from(le32(&marker, 8)?);
        if stored == 0 || stored > 4 * self.grain {
            return Err(corrupt("VMDK compressed grain has an invalid size"));
        }
        Ok(Extent {
            len,
            kind: Kind::Packed(Packed {
                file: self.file,
                offset: at + MARKER_LEN,
                stored: stored as usize,
                codec: Codec::Zlib,
                start: offset - within,
                plain: self.grain as usize,
            }),
        })
    }
}

impl Mapping for Vmdk {
    fn size(&self) -> u64 {
        self.size
    }

    fn map(&self, offset: u64, files: &mut dyn Files) -> io::Result<Extent> {
        let index = self
            .parts
            .partition_point(|part| part.start + part.len <= offset);
        let part = self
            .parts
            .get(index)
            .ok_or_else(|| corrupt("VMDK offset beyond its extents"))?;
        let into = offset - part.start;
        let rest = part.len - into;
        match &part.kind {
            PartKind::Zero => Ok(Extent {
                len: rest,
                kind: Kind::Zero,
            }),
            PartKind::Flat { file, offset } => Ok(Extent {
                len: rest,
                kind: Kind::Data {
                    file: *file,
                    offset: offset
                        .checked_add(into)
                        .ok_or_else(|| corrupt("VMDK flat extent offset is out of range"))?,
                },
            }),
            PartKind::Sparse(sparse) => {
                let mut extent = sparse.map(into, files)?;
                extent.len = extent.len.min(rest);
                if let Kind::Packed(packed) = &mut extent.kind {
                    packed.start += part.start;
                }
                Ok(extent)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::super::tests::{container, fixture, opened, read_all, refusal, try_disk};
    use super::*;

    #[test]
    fn parses_descriptors() {
        let text = "# Disk DescriptorFile\r\nVersion=1\r\nCID=ca8ea40a\r\nParentCID=FFFFFFFF\r\n\
            CreateType = \"twoGbMaxExtentFlat\"\r\n\r\n# Extent description\r\n\
            RW 8 flat \"a b.vmdk\" 8\r\nRW 16 SPARSE \"s.vmdk\"\r\nRW 4096 ZERO\r\n\
            rdonly 8 FLAT \"a=b-flat.vmdk\" 0\r\n\
            \r\n#DDB\r\nddb.geometry.heads = \"16\"\r\n\0\0garbage";
        let parsed = descriptor(&text_of(text.as_bytes())).unwrap();
        assert_eq!(parsed.create_type.as_deref(), Some("twoGbMaxExtentFlat"));
        assert_eq!(parsed.extents.len(), 4);
        // A `=` in an extent name doesn't turn the line into a key.
        assert_eq!(parsed.extents[3].name.as_deref(), Some("a=b-flat.vmdk"));
        let flat = &parsed.extents[0];
        assert_eq!(
            (flat.sectors, flat.kind, flat.name.as_deref(), flat.offset),
            (8, LineKind::Flat, Some("a b.vmdk"), 8)
        );
        assert_eq!(parsed.extents[1].kind, LineKind::Sparse);
        assert_eq!(parsed.extents[1].offset, 0);
        assert_eq!(
            (parsed.extents[2].kind, parsed.extents[2].name.as_deref()),
            (LineKind::Zero, None)
        );

        let refused = |text: &str| match descriptor(text) {
            Err(Error::Image(reason)) => reason,
            other => panic!("{other:?}"),
        };
        let reason = refused("parentCID=5ef2d7db\nparentFileNameHint=\"base/disk.vmdk\"\n");
        assert!(reason.contains("parent") && reason.contains("“base/disk.vmdk”"));
        assert!(refused("parentCID=00000000\n").contains("parent"));
        assert!(refused("createType=\"vmfsSparse\"\n").contains("“vmfsSparse” can't be browsed"));
        assert!(refused("RW 8 VMFSSPARSE \"x.vmdk\"\n").contains("VMFSSPARSE"));
        assert!(refused("RW 8 FLAT \"../x.vmdk\" 0\n").contains("path"));
        assert!(refused("RW 8 FLAT \"c:\\x.vmdk\" 0\n").contains("path"));
        assert!(refused("RW 8 FLAT\n").contains("names no file"));
        assert!(refused("RW many FLAT \"x\"\n").contains("understood"));
        assert!(descriptor("createType=\"VMFS\"\nparentCID=ffffffff\n").is_ok());
    }

    fn header(version: u32, flags: u32, capacity: u64, directory: u64) -> Vec<u8> {
        let mut header = vec![0; HEADER_LEN];
        header[..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&version.to_le_bytes());
        header[8..12].copy_from_slice(&flags.to_le_bytes());
        header[12..20].copy_from_slice(&capacity.to_le_bytes());
        header[20..28].copy_from_slice(&8u64.to_le_bytes());
        header[44..48].copy_from_slice(&512u32.to_le_bytes());
        header[56..64].copy_from_slice(&directory.to_le_bytes());
        header[73..77].copy_from_slice(b"\n \r\n");
        header[77..79].copy_from_slice(&COMPRESS_DEFLATE.to_le_bytes());
        header
    }

    /// 4 sectors of grain table entries.
    fn grain_table(entries: &[u32]) -> Vec<u8> {
        let mut table: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
        table.resize(2048, 0);
        table
    }

    fn sector(data: &[u8]) -> Vec<u8> {
        let mut sector = data.to_vec();
        sector.resize(sector.len().div_ceil(512) * 512, 0);
        sector
    }

    #[test]
    fn reads_a_sparse_extent() {
        // Header, directory at 1, table at 2..6, grain 2 at 6..14 (8 sectors of 4 KiB).
        let mut file = header(1, 3, 24, 1);
        file.extend(sector(&2u32.to_le_bytes()));
        file.extend(grain_table(&[0, ZEROED, 6]));
        file.extend(vec![9; 4096]);
        let mut disk = opened(file);
        assert_eq!(disk.size(), 24 * 512);
        let data = read_all(&mut disk);
        assert!(data[..8192].iter().all(|b| *b == 0));
        assert!(data[8192..].iter().all(|b| *b == 9));
    }

    /// A stream-optimized file with its directory in the footer, as VMware writes it.
    fn stream(lba_of_second_grain: u64, flags: u32) -> (Vec<u8>, Vec<u8>) {
        let grains: Vec<Vec<u8>> = (0..2u8)
            .map(|g| {
                (0..4096u32)
                    .map(|i| (i as u8).wrapping_mul(g + 1))
                    .collect()
            })
            .collect();
        let mut file = header(3, flags, 16, GD_AT_END);
        file.extend([0; 512]); // Where the descriptor would be; sector 1 means "zeroed grain".
        let mut sectors = Vec::new();
        for (g, grain) in grains.iter().enumerate() {
            sectors.push(file.len() as u64 / 512);
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(grain, 6);
            let lba = if g == 1 { lba_of_second_grain } else { 0 };
            let mut marker = lba.to_le_bytes().to_vec();
            marker.extend((packed.len() as u32).to_le_bytes());
            marker.extend(packed);
            file.extend(sector(&marker));
        }
        let table = file.len() as u64 / 512;
        file.extend(grain_table(&[sectors[0] as u32, sectors[1] as u32]));
        let directory = file.len() as u64 / 512;
        file.extend(sector(&(table as u32).to_le_bytes()));
        file.extend([0; 512]); // Footer marker.
        file.extend(header(3, flags, 16, directory));
        file.extend([0; 512]); // End-of-stream marker.
        (file, grains.concat())
    }

    #[test]
    fn reads_a_stream_with_the_directory_at_the_end() {
        let (file, plain) = stream(8, FLAG_COMPRESSED | 0x3_0003);
        let mut disk = opened(file);
        assert_eq!(disk.size(), 8192);
        assert_eq!(read_all(&mut disk), plain);

        let (file, _) = stream(16, FLAG_COMPRESSED | 0x3_0003);
        let err = read_all_err(opened(file));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let (mut file, _) = stream(8, FLAG_COMPRESSED);
        let end = file.len() - 1024;
        file[77..79].fill(0);
        file[end + 77..end + 79].fill(0);
        assert!(refusal(file).contains("algorithm"));
    }

    fn read_all_err(mut disk: super::super::Disk<Cursor<Vec<u8>>>) -> io::Error {
        disk.read_to_end(&mut Vec::new()).unwrap_err()
    }

    #[test]
    fn stitches_extents() {
        let text = b"# Disk DescriptorFile\nversion=1\nCID=1\nparentCID=ffffffff\n\
            createType=\"twoGbMaxExtentFlat\"\nRW 8 FLAT \"a.bin\" 2\nRW 16 ZERO\nRW 8 FLAT \"b.bin\" 0\n"
            .to_vec();
        let a: Vec<u8> = (0..10 * 512).map(|i| (i / 512) as u8).collect();
        let b = vec![7; 8 * 512];
        let sibling = move |name: &str| {
            Ok(match name {
                "a.bin" => Some(Cursor::new(a.clone())),
                "b.bin" => Some(Cursor::new(b.clone())),
                _ => None,
            })
        };
        let mut disk = super::super::open(Cursor::new(text.clone()), &sibling).unwrap();
        assert_eq!(disk.size(), 32 * 512);
        let data = read_all(&mut disk);
        let expected: Vec<u8> = (2..10u8)
            .flat_map(|s| [s; 512])
            .chain([0; 16 * 512])
            .chain([7; 8 * 512])
            .collect();
        assert_eq!(data, expected);

        let mut missing = text;
        missing.extend(b"RW 8 FLAT \"c.bin\" 0\n");
        match super::super::open(Cursor::new(missing), &sibling) {
            Err(Error::Image(reason)) => assert!(reason.contains("“c.bin” is not in the snapshot")),
            other => panic!("{:?}", other.map(|d| d.size())),
        }
    }

    #[test]
    fn decodes_the_fixtures() {
        let ntfs = fixture("ntfs.img");
        for name in ["sparse.vmdk", "stream.vmdk"] {
            let mut disk = container(name);
            assert_eq!(disk.size(), ntfs.len() as u64, "{name}");
            assert!(read_all(&mut disk) == ntfs, "{name} differs");
        }
        assert!(refusal(fixture("containers/child.vmdk")).contains("parent"));
        assert!(try_disk(b"COWD".repeat(128)).is_err());
    }
}
