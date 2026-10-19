//! Files compressed by the Windows Overlay Filter ("system compression", CompactOS).
//!
//! Such a file has a reparse point with tag [`REPARSE_TAG`], a sparse unnamed data
//! stream of the uncompressed size and the data in the stream [`STREAM_NAME`]:
//! a table with the end of every chunk but the last, then the chunks. Every chunk
//! is compressed on its own, or stored raw if that isn't smaller.

use std::io::{self, Read, Seek, SeekFrom};

use super::lzx;

pub const REPARSE_TAG: u32 = 0x8000_0017;
pub const STREAM_NAME: &str = "WofCompressedData";

/// `WOF_PROVIDER_FILE`; provider 1 is WIMBoot, which keeps the data in a WIM file.
const PROVIDER_FILE: u32 = 2;
const PROVIDER_WIM: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Xpress4k,
    Lzx,
    Xpress8k,
    Xpress16k,
}

impl Format {
    fn chunk_size(self) -> usize {
        match self {
            Self::Xpress4k => 4096,
            Self::Xpress8k => 8192,
            Self::Xpress16k => 16384,
            Self::Lzx => lzx::CHUNK_SIZE,
        }
    }

    fn decompress(self, chunk: &[u8], len: usize) -> io::Result<Vec<u8>> {
        let data = match self {
            Self::Lzx => lzx::decompress_chunk(chunk, len).map_err(|err| corrupt(&err))?,
            _ => xpress_huffman::decompress(chunk, len).map_err(|err| corrupt(&err))?,
        };
        if data.len() != len {
            return Err(corrupt(&"chunk is shorter than expected"));
        }
        Ok(data)
    }
}

/// The compression format from the reparse point of a WOF file, header included.
///
/// Fails with a message for files that aren't compressed by WOF itself.
pub fn parse_reparse_point(data: &[u8]) -> Result<Format, String> {
    let u32_at = |offset: usize| {
        data.get(offset..offset + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    if u32_at(0) != Some(REPARSE_TAG) {
        return Err("not a WOF reparse point".into());
    }
    // Tag and data length, then `WOF_EXTERNAL_INFO` and the file provider's info.
    let (Some(version), Some(provider), Some(file_version), Some(format)) =
        (u32_at(8), u32_at(12), u32_at(16), u32_at(20))
    else {
        return Err("truncated WOF reparse point".into());
    };
    match (version, provider, file_version, format) {
        (1, PROVIDER_FILE, 1, 0) => Ok(Format::Xpress4k),
        (1, PROVIDER_FILE, 1, 1) => Ok(Format::Lzx),
        (1, PROVIDER_FILE, 1, 2) => Ok(Format::Xpress8k),
        (1, PROVIDER_FILE, 1, 3) => Ok(Format::Xpress16k),
        (1, PROVIDER_WIM, ..) => Err("stored in a WIM file outside the volume (WIMBoot)".into()),
        _ => Err(format!(
            "unknown Windows compression (provider {provider}, format {format})"
        )),
    }
}

/// The uncompressed contents of a WOF file, read from its compressed stream.
pub struct WofReader<R> {
    stream: R,
    stream_len: u64,
    format: Format,
    size: u64,
    chunks: u64,
    /// Width of a chunk table entry: 8 bytes for files of 4 GiB and more.
    entry_size: u64,
    pos: u64,
    /// The chunk decompressed last, so small sequential reads don't decompress it again.
    cached: Option<(u64, Vec<u8>)>,
}

impl<R: Read + Seek> WofReader<R> {
    /// `stream` is the compressed stream of `stream_len` bytes, `size` the uncompressed size.
    pub fn new(stream: R, stream_len: u64, format: Format, size: u64) -> Self {
        let chunk_size = format.chunk_size() as u64;
        Self {
            stream,
            stream_len,
            format,
            size,
            chunks: size.div_ceil(chunk_size),
            entry_size: if size >= 1 << 32 { 8 } else { 4 },
            pos: 0,
            cached: None,
        }
    }

    fn table_len(&self) -> u64 {
        self.chunks.saturating_sub(1) * self.entry_size
    }

    /// The end of chunk `index` relative to the end of the table.
    fn chunk_end(&mut self, index: u64) -> io::Result<u64> {
        if index + 1 == self.chunks {
            return self
                .stream_len
                .checked_sub(self.table_len())
                .ok_or_else(|| corrupt(&"stream is shorter than its chunk table"));
        }
        let mut entry = [0; 8];
        self.stream.seek(SeekFrom::Start(index * self.entry_size))?;
        self.stream
            .read_exact(&mut entry[..self.entry_size as usize])?;
        Ok(u64::from_le_bytes(entry))
    }

    fn load_chunk(&mut self, index: u64) -> io::Result<Vec<u8>> {
        let chunk_size = self.format.chunk_size() as u64;
        let len = (self.size - index * chunk_size).min(chunk_size);
        let start = if index == 0 {
            0
        } else {
            self.chunk_end(index - 1)?
        };
        let end = self.chunk_end(index)?;
        let stored = end
            .checked_sub(start)
            .filter(|stored| (1..=len).contains(stored))
            .ok_or_else(|| corrupt(&format!("chunk {index} has an invalid size")))?;

        let mut data = vec![0; stored as usize];
        self.stream
            .seek(SeekFrom::Start(self.table_len() + start))?;
        self.stream.read_exact(&mut data)?;
        if stored == len {
            Ok(data)
        } else {
            self.format.decompress(&data, len as usize)
        }
    }
}

impl<R: Read + Seek> Read for WofReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let chunk_size = self.format.chunk_size() as u64;
        let index = self.pos / chunk_size;
        if self.cached.as_ref().is_none_or(|(i, _)| *i != index) {
            let data = self.load_chunk(index)?;
            self.cached = Some((index, data));
        }
        let (_, data) = self.cached.as_ref().unwrap();
        let offset = (self.pos - index * chunk_size) as usize;
        let n = buf.len().min(data.len() - offset);
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for WofReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.size, pos)?;
        Ok(self.pos)
    }
}

/// The new position after `pos`, for a stream at `current` that is `len` bytes long.
pub(super) fn seek_position(current: u64, len: u64, pos: SeekFrom) -> io::Result<u64> {
    let new = match pos {
        SeekFrom::Start(offset) => Some(offset),
        SeekFrom::End(delta) => len.checked_add_signed(delta),
        SeekFrom::Current(delta) => current.checked_add_signed(delta),
    };
    new.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))
}

fn corrupt(err: &dyn std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Corrupt Windows-compressed file: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/codecs")
                .join(name),
        )
        .unwrap()
    }

    fn reader(name: &str, format: Format) -> (WofReader<Cursor<Vec<u8>>>, Vec<u8>) {
        let stream = fixture(name);
        let plain = fixture("plain.bin");
        let len = stream.len() as u64;
        (
            WofReader::new(Cursor::new(stream), len, format, plain.len() as u64),
            plain,
        )
    }

    #[test]
    fn decompresses_every_format() {
        for (name, format) in [
            ("xpress4k.wof", Format::Xpress4k),
            ("xpress8k.wof", Format::Xpress8k),
            ("xpress16k.wof", Format::Xpress16k),
            ("lzx.wof", Format::Lzx),
        ] {
            let (mut reader, plain) = reader(name, format);
            let mut data = Vec::new();
            reader.read_to_end(&mut data).unwrap();
            assert!(data == plain, "{name} differs");
        }
    }

    #[test]
    fn seeks_into_the_middle_of_a_chunk() {
        let (mut reader, plain) = reader("lzx.wof", Format::Lzx);
        for start in [100_000, 40_000, 5, 32_767] {
            reader.seek(SeekFrom::Start(start)).unwrap();
            let mut data = vec![0; 1000];
            let n = reader.read(&mut data).unwrap();
            assert_eq!(&data[..n], &plain[start as usize..start as usize + n]);
        }
        reader.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(reader.read(&mut [0; 10]).unwrap(), 0);
    }

    #[test]
    fn rejects_a_damaged_chunk_table() {
        let mut stream = fixture("xpress4k.wof");
        stream[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let len = stream.len() as u64;
        let mut reader = WofReader::new(Cursor::new(stream), len, Format::Xpress4k, 110_000);
        let err = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parses_reparse_points() {
        let point = |provider: u32, format: u32| {
            [REPARSE_TAG, 16, 1, provider, 1, format]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        assert_eq!(parse_reparse_point(&point(2, 1)), Ok(Format::Lzx));
        assert_eq!(parse_reparse_point(&point(2, 3)), Ok(Format::Xpress16k));
        assert!(
            parse_reparse_point(&point(1, 0))
                .unwrap_err()
                .contains("WIMBoot")
        );
        assert!(parse_reparse_point(&point(2, 9)).is_err());
        assert!(parse_reparse_point(&point(2, 0)[..12]).is_err());
    }
}
