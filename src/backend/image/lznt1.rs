//! Files with NTFS compression ("compress contents to save disk space", LZNT1).
//!
//! The data is split into compression units of 16 clusters. A unit whose clusters
//! are all sparse holds zeros, one with all clusters allocated is stored raw, and
//! one with only its first clusters allocated holds LZNT1 data: chunks of 4 KiB
//! uncompressed, each with a 2-byte header. A compressed chunk that decodes to
//! less than 4 KiB is followed by zeros, a header of 0 ends the unit.

use std::io::{self, Read, Seek, SeekFrom};

use super::wof::seek_position;

/// Clusters per compression unit; NTFS uses no other size.
const UNIT_CLUSTERS: u64 = 16;
/// Uncompressed size of an LZNT1 chunk.
const CHUNK_SIZE: usize = 4096;

/// Where the clusters of a compressed stream are.
pub trait Clusters {
    /// The position on the volume of the cluster at `offset` in the stream, `None` if sparse.
    fn position(&mut self, offset: u64) -> io::Result<Option<u64>>;

    /// Reads `buf.len()` bytes at `position` on the volume.
    fn read_at(&mut self, position: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// The uncompressed contents of an NTFS-compressed stream.
pub struct Lznt1Reader<C> {
    clusters: C,
    cluster_size: u64,
    size: u64,
    pos: u64,
    /// The unit decompressed last, so small sequential reads don't decompress it again.
    cached: Option<(u64, Vec<u8>)>,
}

impl<C: Clusters> Lznt1Reader<C> {
    pub fn new(clusters: C, cluster_size: u64, size: u64) -> Self {
        Self {
            clusters,
            cluster_size,
            size,
            pos: 0,
            cached: None,
        }
    }

    fn unit_size(&self) -> u64 {
        self.cluster_size * UNIT_CLUSTERS
    }

    fn load_unit(&mut self, index: u64) -> io::Result<Vec<u8>> {
        let start = index * self.unit_size();
        let len = (self.size - start).min(self.unit_size()) as usize;

        let mut positions = Vec::with_capacity(UNIT_CLUSTERS as usize);
        for cluster in 0..UNIT_CLUSTERS {
            positions.push(
                self.clusters
                    .position(start + cluster * self.cluster_size)?,
            );
        }
        let allocated = positions.iter().take_while(|p| p.is_some()).count();
        if positions[allocated..].iter().any(Option::is_some) {
            return Err(corrupt("a compression unit has a gap"));
        }

        let mut stored = vec![0; allocated * self.cluster_size as usize];
        for (cluster, position) in stored
            .chunks_mut(self.cluster_size as usize)
            .zip(positions.iter().flatten())
        {
            self.clusters.read_at(*position, cluster)?;
        }

        let mut data = match allocated as u64 {
            0 => Vec::new(),
            UNIT_CLUSTERS => stored,
            _ => decompress_unit(&stored, len)?,
        };
        data.resize(len, 0);
        Ok(data)
    }
}

/// Decompresses the LZNT1 chunks of one unit, up to `len` bytes.
fn decompress_unit(stored: &[u8], len: usize) -> io::Result<Vec<u8>> {
    let mut data = Vec::with_capacity(len);
    let mut rest = stored;
    while data.len() < len && rest.len() >= 2 {
        let header = u16::from_le_bytes([rest[0], rest[1]]);
        if header == 0 {
            break;
        }
        let chunk_len = 2 + (header & 0x0FFF) as usize + 1;
        let chunk = rest
            .get(..chunk_len)
            .ok_or_else(|| corrupt("a chunk runs past its compression unit"))?;
        let before = data.len();
        lznt1::decompress(chunk, &mut data).map_err(|err| corrupt(&err.to_string()))?;
        if data.len() - before > CHUNK_SIZE {
            return Err(corrupt("a chunk decodes to more than 4 KiB"));
        }
        // Every chunk but the last stands for 4 KiB, a shorter one is padded with zeros.
        data.resize(before + CHUNK_SIZE, 0);
        rest = &rest[chunk_len..];
    }
    data.truncate(len);
    Ok(data)
}

impl<C: Clusters> Read for Lznt1Reader<C> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let index = self.pos / self.unit_size();
        if self.cached.as_ref().is_none_or(|(i, _)| *i != index) {
            let data = self.load_unit(index)?;
            self.cached = Some((index, data));
        }
        let (_, data) = self.cached.as_ref().unwrap();
        let offset = (self.pos - index * self.unit_size()) as usize;
        let n = buf.len().min(data.len() - offset);
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<C: Clusters> Seek for Lznt1Reader<C> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.size, pos)?;
        Ok(self.pos)
    }
}

fn corrupt(message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Corrupt NTFS-compressed file: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLUSTER: u64 = 512;
    const UNIT: usize = (CLUSTER * UNIT_CLUSTERS) as usize;

    /// A stream laid out like on disk: one entry per cluster, `None` for sparse ones.
    struct FakeClusters(Vec<Option<Vec<u8>>>);

    impl Clusters for FakeClusters {
        fn position(&mut self, offset: u64) -> io::Result<Option<u64>> {
            let index = (offset / CLUSTER) as usize;
            Ok(self
                .0
                .get(index)
                .and_then(|c| c.as_ref())
                .map(|_| index as u64 * CLUSTER))
        }

        fn read_at(&mut self, position: u64, buf: &mut [u8]) -> io::Result<()> {
            let cluster = self.0[(position / CLUSTER) as usize].as_ref().unwrap();
            buf.copy_from_slice(cluster);
            Ok(())
        }
    }

    /// Splits `stored` into clusters and pads the unit with sparse ones.
    fn unit(stored: &[u8]) -> Vec<Option<Vec<u8>>> {
        let mut clusters: Vec<_> = stored
            .chunks(CLUSTER as usize)
            .map(|c| {
                let mut c = c.to_vec();
                c.resize(CLUSTER as usize, 0);
                Some(c)
            })
            .collect();
        clusters.resize(UNIT_CLUSTERS as usize, None);
        clusters
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        lznt1::compress(data, &mut out);
        out
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| b"ntfs compression unit "[i % 22])
            .collect()
    }

    #[test]
    fn reads_compressed_raw_and_sparse_units() {
        let first = sample(UNIT);
        let raw: Vec<u8> = (0..UNIT).map(|i| (i * 7919 % 251) as u8).collect();
        let last = sample(1000);

        let mut clusters = unit(&compress(&first));
        clusters.extend(raw.chunks(CLUSTER as usize).map(|c| Some(c.to_vec())));
        clusters.extend(unit(&[]));
        clusters.extend(unit(&compress(&last)));

        let size = (3 * UNIT + last.len()) as u64;
        let mut reader = Lznt1Reader::new(FakeClusters(clusters), CLUSTER, size);
        let mut data = Vec::new();
        reader.read_to_end(&mut data).unwrap();

        let mut expected = first;
        expected.extend(&raw);
        expected.extend(vec![0; UNIT]);
        expected.extend(&last);
        assert!(data == expected);

        reader.seek(SeekFrom::Start(UNIT as u64 + 10)).unwrap();
        let mut buf = [0; 5];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, raw[10..15]);
    }

    #[test]
    fn pads_short_chunks_with_zeros() {
        // A chunk that decodes to 100 bytes stands for 4 KiB, the next one follows after it.
        let mut stored = compress(&sample(100));
        stored.extend(compress(b"second chunk"));
        let mut reader = Lznt1Reader::new(FakeClusters(unit(&stored)), CLUSTER, 4096 + 12);
        let mut data = Vec::new();
        reader.read_to_end(&mut data).unwrap();
        assert_eq!(&data[..100], &sample(100)[..]);
        assert!(data[100..4096].iter().all(|&b| b == 0));
        assert_eq!(&data[4096..], b"second chunk");
    }

    #[test]
    fn rejects_corrupt_units() {
        // A chunk header claiming more data than the unit has.
        let reader_for =
            |stored: &[u8]| Lznt1Reader::new(FakeClusters(unit(stored)), CLUSTER, UNIT as u64);
        let err = reader_for(&[0xFF, 0xBF, 1, 2, 3])
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // An allocated cluster after a sparse one.
        let mut clusters = unit(&compress(&sample(100)));
        clusters[5] = Some(vec![0; CLUSTER as usize]);
        let err = Lznt1Reader::new(FakeClusters(clusters), CLUSTER, UNIT as u64)
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
