//! Container formats of VM disks (qcow2, VHD, VHDX, VMDK), read like raw disks.
//!
//! Such a file keeps the virtual disk in clusters, blocks or grains and says in
//! tables where each of them is, if anywhere. [`Disk`] reads the file as the
//! virtual disk it describes: every read asks the format's [`Mapping`] what backs
//! the bytes at the position (nothing, plain data, or part of a compressed cluster)
//! and fetches them from the image file, or from one of the extent files of a VMDK.
//! Images that need another image (backing files, differencing disks, snapshots)
//! are refused with the reason.

mod qcow2;
mod vhd;
mod vhdx;
mod vmdk;

use std::{
    fmt::Display,
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};

use super::wof::seek_position;
use crate::backend::{Error, Result};

/// Largest table loaded into memory as a whole; QEMU's limit for the qcow2 L1 table too.
const MAX_TABLE: usize = 32 << 20;

const SECTOR: u64 = 512;

/// What backs a run of bytes of the virtual disk.
///
/// `len` never crosses a cluster, grain, block or extent boundary.
struct Extent {
    len: u64,
    kind: Kind,
}

enum Kind {
    Zero,
    /// Stored as is at `offset` in file number `file`.
    Data {
        file: usize,
        offset: u64,
    },
    Packed(Packed),
}

/// Part of a compressed cluster or grain: `stored` bytes at `offset` in file number
/// `file` decode to the cluster that starts at virtual offset `start` and has at
/// most `plain` bytes (the cluster size).
#[derive(Clone, Copy)]
struct Packed {
    file: usize,
    offset: u64,
    stored: usize,
    codec: Codec,
    start: u64,
    plain: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    /// Raw deflate without a header: qcow2.
    Deflate,
    /// Deflate with a zlib header: VMDK.
    Zlib,
    /// One zstd frame, padding after it: qcow2.
    Zstd,
}

/// The tables of one format: where every byte of the virtual disk is stored.
trait Mapping: Send + Sync {
    fn size(&self) -> u64;

    /// The run at `offset` (below `size`). Tables not in memory are read through `files`.
    fn map(&self, offset: u64, files: &mut dyn Files) -> io::Result<Extent>;
}

/// The files of a disk: one, or several VMDK extent files.
trait Files {
    /// Fills `buf` from `offset` of file `file`; short only at the end of the file.
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    fn len(&mut self, file: usize) -> io::Result<u64>;

    fn read_exact_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.read_at(file, offset, buf)? < buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the image file",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Sources<R>(Vec<R>);

impl<R: Read + Seek> Sources<R> {
    fn file(&mut self, file: usize) -> io::Result<&mut R> {
        self.0
            .get_mut(file)
            .ok_or_else(|| corrupt(format!("extent file {file} isn't open")))
    }
}

impl<R: Read + Seek> Files for Sources<R> {
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let reader = self.file(file)?;
        reader.seek(SeekFrom::Start(offset))?;
        let mut done = 0;
        while done < buf.len() {
            match reader.read(&mut buf[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(done)
    }

    fn len(&mut self, file: usize) -> io::Result<u64> {
        self.file(file)?.seek(SeekFrom::End(0))
    }
}

/// A raw image: the file is the disk.
struct Raw(u64);

impl Mapping for Raw {
    fn size(&self) -> u64 {
        self.0
    }

    fn map(&self, offset: u64, _: &mut dyn Files) -> io::Result<Extent> {
        Ok(Extent {
            len: self.0 - offset,
            kind: Kind::Data { file: 0, offset },
        })
    }
}

/// Decoded clusters kept per image. A file system doesn't read a file front to
/// back only: FAT walks the FAT table between two reads of a data cluster, and
/// directory clusters come in between; one entry would then decode the same data
/// cluster again for every read. Eight cover that with room to spare, at most
/// 8 x 2 MiB (the largest qcow2 cluster) per open image.
const PACKED_CLUSTERS_CACHED: usize = 8;

/// What all clones of a [`Disk`] share: the tables and the clusters decoded recently.
struct Shared {
    mapping: Box<dyn Mapping>,
    /// Keyed by the cluster's virtual start.
    packed: TableCache,
}

impl Shared {
    fn new(mapping: Box<dyn Mapping>) -> Self {
        Self {
            mapping,
            packed: TableCache::new(PACKED_CLUSTERS_CACHED),
        }
    }

    fn cluster(&self, files: &mut dyn Files, packed: Packed) -> io::Result<Arc<[u8]>> {
        self.packed.get(packed.start, || {
            // The last cluster's stored length may reach past the end of the file.
            let mut stored = vec![0; packed.stored];
            let n = files.read_at(packed.file, packed.offset, &mut stored)?;
            stored.truncate(n);
            decode(packed.codec, &stored, packed.plain)
        })
    }
}

/// At most `limit` bytes of the decompressed `data`.
fn decode(codec: Codec, data: &[u8], limit: usize) -> io::Result<Vec<u8>> {
    let mut plain = match codec {
        Codec::Deflate => miniz_oxide::inflate::decompress_to_vec_with_limit(data, limit)
            .map_err(|err| corrupt(format!("{:?}", err.status)))?,
        Codec::Zlib => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, limit)
            .map_err(|err| corrupt(format!("{:?}", err.status)))?,
        Codec::Zstd => {
            let mut plain = Vec::new();
            zstd::stream::read::Decoder::new(data)?
                .take(limit as u64)
                .read_to_end(&mut plain)?;
            plain
        }
    };
    plain.truncate(limit);
    Ok(plain)
}

/// The virtual disk of an image file, `Read + Seek` like a raw image.
///
/// Clones share the tables and keep their own position.
pub struct Disk<R> {
    shared: Arc<Shared>,
    files: Sources<R>,
    pos: u64,
}

impl<R: Clone> Clone for Disk<R> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            files: self.files.clone(),
            pos: self.pos,
        }
    }
}

impl<R> Disk<R> {
    pub fn size(&self) -> u64 {
        self.shared.mapping.size()
    }
}

impl<R: Read + Seek> Read for Disk<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.size();
        if self.pos >= size || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min((size - self.pos) as usize);
        let extent = self.shared.mapping.map(self.pos, &mut self.files)?;
        if extent.len == 0 {
            return Err(corrupt("empty extent"));
        }
        // Never read across an extent boundary; callers use `read_exact`.
        let n = want.min(extent.len as usize);
        match extent.kind {
            Kind::Zero => buf[..n].fill(0),
            Kind::Data { file, offset } => self.files.read_exact_at(file, offset, &mut buf[..n])?,
            Kind::Packed(packed) => {
                let cluster = self.shared.cluster(&mut self.files, packed)?;
                let at = (self.pos - packed.start) as usize;
                let data = cluster
                    .get(at..at + n)
                    .ok_or_else(|| corrupt("compressed cluster is shorter than expected"))?;
                buf[..n].copy_from_slice(data);
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for Disk<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.size(), pos)?;
        Ok(self.pos)
    }
}

/// Recently used tables read on demand (qcow2 L2 tables, VMDK grain tables) and
/// decoded clusters, which are an LRU of byte blocks keyed by offset too.
struct TableCache {
    /// Most recently used last.
    tables: Mutex<Vec<(u64, Arc<[u8]>)>>,
    capacity: usize,
}

impl TableCache {
    fn new(capacity: usize) -> Self {
        Self {
            tables: Mutex::new(Vec::new()),
            capacity: capacity.max(1),
        }
    }

    /// The table at `key`; `load` runs without the lock held, so a concurrent miss may load twice.
    fn get(&self, key: u64, load: impl FnOnce() -> io::Result<Vec<u8>>) -> io::Result<Arc<[u8]>> {
        {
            let mut tables = self.tables.lock().unwrap();
            if let Some(index) = tables.iter().position(|(k, _)| *k == key) {
                let table = tables.remove(index);
                tables.push(table.clone());
                return Ok(table.1);
            }
        }
        let table: Arc<[u8]> = load()?.into();
        let mut tables = self.tables.lock().unwrap();
        tables.retain(|(k, _)| *k != key);
        tables.push((key, table.clone()));
        if tables.len() > self.capacity {
            tables.remove(0);
        }
        Ok(table)
    }
}

/// The disk behind `file`. `sibling` opens the file `name` of the image's folder (VMDK extents).
pub(super) fn open<R: Read + Seek + Clone>(
    file: R,
    sibling: &dyn Fn(&str) -> Result<Option<R>>,
) -> Result<Disk<R>> {
    let mut files = Sources(vec![file]);
    let len = files.len(0)?;
    let mut head = [0; SECTOR as usize];
    let n = files.read_at(0, 0, &mut head)?;
    let mut tail = [0; SECTOR as usize];
    let tail = if len >= SECTOR {
        files.read_exact_at(0, len - SECTOR, &mut tail)?;
        &tail[..]
    } else {
        &[]
    };
    let mapping: Box<dyn Mapping> = match sniff(&head[..n], tail) {
        Format::Qcow2 => qcow2::open(&mut files)?,
        Format::Vhdx => vhdx::open(&mut files)?,
        Format::VmdkSparse => vmdk::open_sparse(&mut files)?,
        Format::VmdkCowd => {
            return Err(Error::Image(
                "VMDK ESX sparse (COWD) disks can't be browsed".into(),
            ));
        }
        Format::VmdkDescriptor => vmdk::open_descriptor(&mut files, len, sibling)?,
        Format::Vhd => vhd::open(&mut files, len)?,
        Format::Raw => Box::new(Raw(len)),
    };
    Ok(Disk {
        shared: Arc::new(Shared::new(mapping)),
        files,
        pos: 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Qcow2,
    Vhdx,
    VmdkSparse,
    VmdkCowd,
    VmdkDescriptor,
    Vhd,
    Raw,
}

/// The format by its magic, from the first and the last sector; the name doesn't count.
///
/// A VHD has its footer at the end (fixed) or at both ends (dynamic); anything
/// without a magic is a raw image, which keeps `-flat.vmdk` extents readable.
fn sniff(head: &[u8], tail: &[u8]) -> Format {
    if head.starts_with(qcow2::MAGIC) {
        Format::Qcow2
    } else if head.starts_with(vhdx::MAGIC) {
        Format::Vhdx
    } else if head.starts_with(vmdk::MAGIC) {
        Format::VmdkSparse
    } else if head.starts_with(vmdk::MAGIC_COWD) {
        Format::VmdkCowd
    } else if head.starts_with(vmdk::DESCRIPTOR_MAGIC) {
        Format::VmdkDescriptor
    } else if head.starts_with(vhd::COOKIE) || tail.starts_with(vhd::COOKIE) {
        Format::Vhd
    } else {
        Format::Raw
    }
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> io::Result<[u8; N]> {
    at.checked_add(N)
        .and_then(|end| bytes.get(at..end))
        .and_then(|field| field.try_into().ok())
        .ok_or_else(|| {
            corrupt(format!(
                "field at {at} lies outside a {}-byte structure",
                bytes.len()
            ))
        })
}

fn be32(bytes: &[u8], at: usize) -> io::Result<u32> {
    array(bytes, at).map(u32::from_be_bytes)
}

fn be64(bytes: &[u8], at: usize) -> io::Result<u64> {
    array(bytes, at).map(u64::from_be_bytes)
}

fn le16(bytes: &[u8], at: usize) -> io::Result<u16> {
    array(bytes, at).map(u16::from_le_bytes)
}

fn le32(bytes: &[u8], at: usize) -> io::Result<u32> {
    array(bytes, at).map(u32::from_le_bytes)
}

fn le64(bytes: &[u8], at: usize) -> io::Result<u64> {
    array(bytes, at).map(u64::from_le_bytes)
}

fn corrupt(what: impl Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Corrupt disk image: {what}"),
    )
}

fn refuse<T>(reason: impl Into<String>) -> Result<T> {
    Err(Error::Image(reason.into()))
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use super::*;

    /// The decompressed fixture `tests/fixtures/<name>.zst`.
    pub(super) fn fixture(name: &str) -> Vec<u8> {
        let file = fs::File::open(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("{name}.zst")),
        )
        .unwrap();
        let mut data = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(file)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        data
    }

    /// The disk of the fixture `tests/fixtures/containers/<name>.zst`.
    pub(super) fn container(name: &str) -> Disk<Cursor<Vec<u8>>> {
        opened(fixture(&format!("containers/{name}")))
    }

    pub(super) fn opened(data: Vec<u8>) -> Disk<Cursor<Vec<u8>>> {
        try_disk(data).unwrap()
    }

    pub(super) fn try_disk(data: Vec<u8>) -> Result<Disk<Cursor<Vec<u8>>>> {
        open(Cursor::new(data), &|_| Ok(None))
    }

    /// The refusal of opening `data`.
    pub(super) fn refusal(data: Vec<u8>) -> String {
        match try_disk(data) {
            Err(Error::Image(reason)) => reason,
            other => panic!("not refused: {:?}", other.map(|d| d.size())),
        }
    }

    pub(super) fn read_all(disk: &mut Disk<Cursor<Vec<u8>>>) -> Vec<u8> {
        let mut data = Vec::new();
        disk.read_to_end(&mut data).unwrap();
        data
    }

    /// Zero and data runs of 4 bytes in turns; data byte = its offset.
    struct Stripes(u64);

    impl Mapping for Stripes {
        fn size(&self) -> u64 {
            self.0
        }

        fn map(&self, offset: u64, _: &mut dyn Files) -> io::Result<Extent> {
            let len = (4 - offset % 4).min(self.0 - offset);
            let kind = if (offset / 4).is_multiple_of(2) {
                Kind::Zero
            } else {
                Kind::Data { file: 0, offset }
            };
            Ok(Extent { len, kind })
        }
    }

    fn disk_of(mapping: impl Mapping + 'static, files: Vec<Vec<u8>>) -> Disk<Cursor<Vec<u8>>> {
        Disk {
            shared: Arc::new(Shared::new(Box::new(mapping))),
            files: Sources(files.into_iter().map(Cursor::new).collect()),
            pos: 0,
        }
    }

    #[test]
    fn reads_one_extent_at_a_time_and_seeks() {
        let model: Vec<u8> = (0..22u8)
            .map(|i| if (i / 4) % 2 == 0 { 0 } else { i })
            .collect();
        let mut disk = disk_of(Stripes(22), vec![(0..22u8).collect()]);
        let mut buf = [0xff; 10];
        assert_eq!(disk.read(&mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], &[0, 0, 0, 0]);
        assert_eq!(disk.read(&mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], &[4, 5, 6, 7]);

        disk.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(read_all(&mut disk), model);
        assert_eq!(disk.seek(SeekFrom::Current(-2)).unwrap(), 20);
        let mut other = disk.clone();
        assert_eq!(disk.seek(SeekFrom::End(-3)).unwrap(), 19);
        assert_eq!(read_all(&mut disk), model[19..]);
        assert_eq!(disk.read(&mut buf).unwrap(), 0);
        // The clone kept its own position.
        assert_eq!(read_all(&mut other), model[20..]);
        assert!(disk.seek(SeekFrom::Current(-100)).is_err());
    }

    /// Counts the reads of each file.
    struct Counting {
        files: Sources<Cursor<Vec<u8>>>,
        reads: usize,
    }

    impl Files for Counting {
        fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.files.read_at(file, offset, buf)
        }

        fn len(&mut self, file: usize) -> io::Result<u64> {
            self.files.len(file)
        }
    }

    /// `plain` compressed with every codec, each as a file with the cluster at
    /// offset 100 and 20 bytes of padding behind it.
    fn packed_files(plain: &[u8]) -> Vec<(Codec, Vec<u8>, Vec<u8>)> {
        [
            (
                Codec::Deflate,
                miniz_oxide::deflate::compress_to_vec(plain, 6),
            ),
            (
                Codec::Zlib,
                miniz_oxide::deflate::compress_to_vec_zlib(plain, 6),
            ),
            (Codec::Zstd, zstd::bulk::compress(plain, 3).unwrap()),
        ]
        .into_iter()
        .map(|(codec, stored)| {
            let mut file = vec![0; 100];
            file.extend_from_slice(&stored);
            file.extend_from_slice(&[0; 20]);
            (codec, stored, file)
        })
        .collect()
    }

    #[test]
    fn decodes_a_packed_cluster_once() {
        let plain: Vec<u8> = (0..3000u32).map(|i| (i % 7) as u8).collect();
        for (codec, stored, file) in packed_files(&plain) {
            // The stored length claims even more than the file has.
            let packed = Packed {
                file: 0,
                offset: 100,
                stored: stored.len() + 1000,
                codec,
                start: 0,
                plain: 3000,
            };
            let shared = Shared::new(Box::new(Raw(0)));
            let mut files = Counting {
                files: Sources(vec![Cursor::new(file)]),
                reads: 0,
            };
            assert_eq!(&*shared.cluster(&mut files, packed).unwrap(), &plain[..]);
            assert_eq!(&*shared.cluster(&mut files, packed).unwrap(), &plain[..]);
            assert_eq!(files.reads, 1, "{codec:?}");
            assert!(decode(codec, &stored[..stored.len() / 2], 3000).is_err());
        }
    }

    /// FAT reads the FAT table between two reads of a data cluster; neither may
    /// be decoded again for that.
    #[test]
    fn keeps_several_packed_clusters() {
        let table: Vec<u8> = (0..3000u32).map(|i| (i % 5) as u8).collect();
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 7) as u8).collect();
        for ((codec, table_stored, table_file), (_, data_stored, data_file)) in
            packed_files(&table).into_iter().zip(packed_files(&data))
        {
            let cluster = |file, stored: &[u8], start| Packed {
                file,
                offset: 100,
                stored: stored.len(),
                codec,
                start,
                plain: 3000,
            };
            let shared = Shared::new(Box::new(Raw(0)));
            let mut files = Counting {
                files: Sources(vec![Cursor::new(table_file), Cursor::new(data_file)]),
                reads: 0,
            };
            for _ in 0..3 {
                let got = shared.cluster(&mut files, cluster(0, &table_stored, 0));
                assert_eq!(&*got.unwrap(), &table[..]);
                let got = shared.cluster(&mut files, cluster(1, &data_stored, 3000));
                assert_eq!(&*got.unwrap(), &data[..]);
            }
            assert_eq!(files.reads, 2, "{codec:?}");
        }
    }

    #[test]
    fn sniffs_the_format() {
        let sector = |magic: &[u8]| {
            let mut sector = vec![0; 512];
            sector[..magic.len()].copy_from_slice(magic);
            sector
        };
        for (magic, format) in [
            (&b"QFI\xfb"[..], Format::Qcow2),
            (b"vhdxfile", Format::Vhdx),
            (b"KDMV", Format::VmdkSparse),
            (b"COWD", Format::VmdkCowd),
            (b"# Disk DescriptorFile\nversion=1", Format::VmdkDescriptor),
            (b"conectix", Format::Vhd),
            (b"\xebR\x90NTFS    ", Format::Raw),
        ] {
            assert_eq!(sniff(&sector(magic), &sector(b"")), format);
        }
        assert_eq!(sniff(&sector(b""), &sector(b"conectix")), Format::Vhd);
        assert_eq!(sniff(b"", b""), Format::Raw);
        assert_eq!(sniff(b"QF", b""), Format::Raw);
        assert!(refusal(sector(b"COWD")).contains("COWD"));
        assert_eq!(opened(b"short".to_vec()).size(), 5);
    }

    #[test]
    fn table_cache_keeps_the_recently_used() {
        let cache = TableCache::new(2);
        let loads = std::cell::Cell::new(0);
        let load = |key: u64| {
            cache.get(key, || {
                loads.set(loads.get() + 1);
                Ok(vec![key as u8])
            })
        };
        assert_eq!(&*load(1).unwrap(), &[1]);
        assert_eq!(&*load(2).unwrap(), &[2]);
        assert_eq!(&*load(1).unwrap(), &[1]);
        assert_eq!(loads.get(), 2);
        load(3).unwrap(); // Evicts 2, used least recently.
        load(1).unwrap();
        load(3).unwrap();
        assert_eq!(loads.get(), 3);
        load(2).unwrap();
        assert_eq!(loads.get(), 4);

        let failed = cache.get(9, || Err(corrupt("no")));
        assert!(failed.is_err());
        assert_eq!(&*load(9).unwrap(), &[9]);
    }
}
