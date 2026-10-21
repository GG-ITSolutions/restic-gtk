//! A read buffer for parsers that seek around a lot.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Bytes fetched per refill. Refills are aligned to it, so scanning backwards
/// (the search for the end of the file list) and forwards (the entries) both hit.
pub const BLOCK: u64 = 1 << 20;

/// Serves reads from the block around the position and refills it when it misses.
///
/// Without it every tiny read of the ZIP parser would be a request to the repository.
/// Clones share the block until one of them refills; each has its own position.
#[derive(Clone)]
pub struct Window<R> {
    inner: R,
    len: u64,
    block: u64,
    pos: u64,
    /// Where `data` starts in the file.
    start: u64,
    data: Arc<[u8]>,
    /// Bytes that refills may still fetch, shared by clones.
    budget: Arc<AtomicU64>,
}

impl<R: Read + Seek> Window<R> {
    /// Wraps `inner`, reading `block` bytes at a time. Reads nothing yet.
    pub fn new(mut inner: R, block: u64) -> io::Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        Ok(Self {
            inner,
            len,
            block: block.max(1),
            pos: 0,
            start: 0,
            data: Arc::from([]),
            budget: Arc::new(AtomicU64::new(u64::MAX)),
        })
    }

    /// Fails refills with [`io::ErrorKind::FileTooLarge`] once `bytes` were fetched
    /// by this window and its clones. Storing `u64::MAX` in the returned handle lifts the limit.
    pub fn with_budget(self, bytes: u64) -> (Self, Arc<AtomicU64>) {
        self.budget.store(bytes, Ordering::Relaxed);
        let budget = self.budget.clone();
        (self, budget)
    }

    fn refill(&mut self) -> io::Result<()> {
        let start = self.pos - self.pos % self.block;
        let want = self.block.min(self.len - start);
        self.budget
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(want)
            })
            .map_err(|_| io::Error::from(io::ErrorKind::FileTooLarge))?;
        let mut data = vec![0; want as usize];
        self.inner.seek(SeekFrom::Start(start))?;
        self.inner.read_exact(&mut data)?;
        self.start = start;
        self.data = data.into();
        Ok(())
    }
}

impl<R: Read + Seek> Read for Window<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        if self.pos < self.start || self.pos - self.start >= self.data.len() as u64 {
            self.refill()?;
        }
        let offset = (self.pos - self.start) as usize;
        let n = out.len().min(self.data.len() - offset);
        out[..n].copy_from_slice(&self.data[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R> Seek for Window<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        self.pos = new
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Cursor, rc::Rc};

    use super::*;

    /// Counts the reads that reach the wrapped reader.
    struct Counting<R>(R, Rc<Cell<usize>>);

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.1.set(self.1.get() + 1);
            self.0.read(buf)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }

    fn data() -> Vec<u8> {
        (0..1000u32).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn reads_across_blocks_like_the_source() {
        let reads = Rc::new(Cell::new(0));
        let mut window = Window::new(Counting(Cursor::new(data()), reads.clone()), 64).unwrap();
        let mut all = Vec::new();
        window.read_to_end(&mut all).unwrap();
        assert_eq!(all, data());
        assert_eq!(reads.get(), 1000usize.div_ceil(64));
    }

    #[test]
    fn seeks_inside_the_block_without_refilling() {
        let reads = Rc::new(Cell::new(0));
        let mut window = Window::new(Counting(Cursor::new(data()), reads.clone()), 64).unwrap();
        let mut buf = [0; 4];
        window.seek(SeekFrom::Start(70)).unwrap();
        window.read_exact(&mut buf).unwrap();
        window.seek(SeekFrom::Start(65)).unwrap();
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data()[65..69]);
        assert_eq!(reads.get(), 1);
        // Backwards into the previous block, and from the end.
        window.seek(SeekFrom::Start(3)).unwrap();
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data()[3..7]);
        window.seek(SeekFrom::End(-4)).unwrap();
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data()[996..]);
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn reads_nothing_past_the_end_and_refuses_seeks_before_the_start() {
        let mut window = Window::new(Cursor::new(data()), 64).unwrap();
        window.seek(SeekFrom::Start(5000)).unwrap();
        assert_eq!(window.read(&mut [0; 8]).unwrap(), 0);
        assert!(window.seek(SeekFrom::Current(-6000)).is_err());
        assert!(window.seek(SeekFrom::End(-1001)).is_err());
    }

    #[test]
    fn clones_keep_their_own_position() {
        let mut a = Window::new(Cursor::new(data()), 64).unwrap();
        a.seek(SeekFrom::Start(100)).unwrap();
        let mut b = a.clone();
        b.seek(SeekFrom::Start(500)).unwrap();
        let (mut x, mut y) = ([0; 2], [0; 2]);
        a.read_exact(&mut x).unwrap();
        b.read_exact(&mut y).unwrap();
        assert_eq!((x[0], y[0]), (data()[100], data()[500]));
    }

    #[test]
    fn the_budget_limits_refills_until_lifted() {
        let (mut window, budget) = Window::new(Cursor::new(data()), 64)
            .unwrap()
            .with_budget(128);
        let mut buf = [0; 1];
        window.seek(SeekFrom::Start(0)).unwrap();
        window.read_exact(&mut buf).unwrap();
        window.seek(SeekFrom::Start(64)).unwrap();
        window.read_exact(&mut buf).unwrap();
        window.seek(SeekFrom::Start(128)).unwrap();
        let err = window.read_exact(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        budget.store(u64::MAX, Ordering::Relaxed);
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], data()[128]);
    }
}
