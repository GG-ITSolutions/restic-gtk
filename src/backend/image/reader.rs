//! Random access to files in a snapshot, and to parts of them.

use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    sync::Arc,
};

use rustic_core::{repofile::Node, vfs::OpenFile};

use super::wof::seek_position;
use crate::backend::{Result, session::IndexedRepo};

/// Reads a file of a snapshot at any position, fetching only the blobs it touches.
///
/// rustic caches recently used blobs per repository, so the many small reads of
/// filesystem parsers don't refetch them. Clones share the opened file but keep
/// their own position.
#[derive(Clone)]
pub struct SnapshotReader {
    repo: Arc<IndexedRepo>,
    file: Arc<OpenFile>,
    size: u64,
    pos: u64,
}

impl SnapshotReader {
    pub fn new(repo: Arc<IndexedRepo>, node: &Node) -> Result<Self> {
        let file = repo.open_file(node)?;
        Ok(Self {
            repo,
            file: Arc::new(file),
            size: node.meta.size,
            pos: 0,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Read for SnapshotReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(self.size.saturating_sub(self.pos) as usize);
        if len == 0 {
            return Ok(0);
        }
        let data = self
            .repo
            .read_file_at(&self.file, self.pos as usize, len)
            .map_err(|err| io::Error::other(err.display_log()))?;
        buf[..data.len()].copy_from_slice(&data);
        self.pos += data.len() as u64;
        Ok(data.len())
    }
}

impl Seek for SnapshotReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.size, pos)?;
        Ok(self.pos)
    }
}

/// `len` bytes of `inner` from `start` on, e.g. a partition of a disk image.
#[derive(Clone)]
pub struct Slice<R> {
    inner: R,
    start: u64,
    len: u64,
    pos: u64,
}

impl<R> Slice<R> {
    pub fn new(inner: R, start: u64, len: u64) -> Self {
        Self {
            inner,
            start,
            len,
            pos: 0,
        }
    }
}

impl<R: Read + Seek> Read for Slice<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(self.len.saturating_sub(self.pos) as usize);
        if len == 0 {
            return Ok(0);
        }
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.inner.read(&mut buf[..len])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for Slice<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.len, pos)?;
        Ok(self.pos)
    }
}

/// Refuses writes, for libraries that want a writable volume but only read here.
pub struct ReadOnly<R>(pub R);

impl<R: Read> Read for ReadOnly<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R: Seek> Seek for ReadOnly<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl<R> Write for ReadOnly<R> {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "disk images in snapshots are read-only",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn slice_reads_only_its_range() {
        let mut slice = Slice::new(Cursor::new((0..100u8).collect::<Vec<_>>()), 10, 20);
        let mut data = Vec::new();
        slice.read_to_end(&mut data).unwrap();
        assert_eq!(data, (10..30u8).collect::<Vec<_>>());

        slice.seek(SeekFrom::End(-5)).unwrap();
        let mut buf = [0; 10];
        assert_eq!(slice.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], &[25, 26, 27, 28, 29]);
    }

    #[test]
    fn read_only_refuses_writes() {
        let mut volume = ReadOnly(Cursor::new(vec![0u8; 4]));
        assert_eq!(
            volume.write(&[1]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
