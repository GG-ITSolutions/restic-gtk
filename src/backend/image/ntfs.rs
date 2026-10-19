//! NTFS volumes, read with the `ntfs` crate plus Windows file compression.

use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

use ntfs::{
    Ntfs, NtfsAttributeFlags, NtfsAttributeType, NtfsError, NtfsFile, NtfsReadSeek,
    attribute_value::NtfsAttributeValue,
    indexes::NtfsFileNameIndex,
    structured_values::{NtfsFileAttributeFlags, NtfsFileNamespace},
};
use rustic_core::jiff::Timestamp;

use super::{
    lznt1::{Clusters, Lznt1Reader},
    wof::{self, WofReader},
};
use crate::backend::{Entry, EntryKind, Error, Result};

/// Record number of the root folder; records below 24 are metadata files.
const ROOT_RECORD: u64 = 5;
const FIRST_USER_RECORD: u64 = 24;

const REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const REPARSE_TAG_DEDUP: u32 = 0x8000_0013;

/// An opened NTFS volume. Holds no reader, every call gets the volume to read from.
pub struct NtfsVolume {
    ntfs: Ntfs,
}

impl NtfsVolume {
    pub fn open<R: Read + Seek>(fs: &mut R) -> Result<Self> {
        let mut ntfs = Ntfs::new(fs)?;
        // Needed for case-insensitive lookups of names.
        ntfs.read_upcase_table(fs)?;
        Ok(Self { ntfs })
    }

    /// The file or folder at `path`, relative to the root of the volume.
    fn file<R: Read + Seek>(&self, fs: &mut R, path: &Path) -> Result<NtfsFile<'_>> {
        let mut file = self.ntfs.root_directory(fs)?;
        let mut walked = PathBuf::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            walked.push(name);
            if !file.is_directory() {
                return Err(Error::NotADirectory(walked));
            }
            let index = file.directory_index(fs)?;
            let mut finder = index.finder();
            let name = name.to_string_lossy();
            let entry = NtfsFileNameIndex::find(&mut finder, &self.ntfs, fs, &name)
                .ok_or_else(|| Error::NotFound(walked.clone()))??;
            let next = entry.to_file(&self.ntfs, fs)?;
            file = next;
        }
        Ok(file)
    }

    /// The entries of the folder at `path`, named below the virtual path `parent`.
    pub fn list<R: Read + Seek>(
        &self,
        fs: &mut R,
        path: &Path,
        parent: &Path,
    ) -> Result<Vec<Entry>> {
        let dir = self.file(fs, path)?;
        if !dir.is_directory() {
            return Err(Error::NotADirectory(parent.to_path_buf()));
        }
        let in_root = dir.file_record_number() == ROOT_RECORD;

        let mut entries = Vec::new();
        let mut links = Vec::new();
        let index = dir.directory_index(fs)?;
        let mut iter = index.entries();
        while let Some(entry) = iter.next(fs) {
            let entry = entry?;
            let Some(key) = entry.key() else { continue };
            let key = key?;
            // 8.3 names duplicate the long name of the same file.
            if key.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            let record = entry.file_reference().file_record_number();
            if in_root && record < FIRST_USER_RECORD {
                continue;
            }
            let name = key.name().to_string_lossy();
            let attributes = key.file_attributes();
            let kind = if key.is_directory() {
                if attributes.contains(NtfsFileAttributeFlags::REPARSE_POINT) {
                    links.push((entries.len(), record));
                }
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            entries.push(Entry {
                path: parent.join(&name),
                name,
                kind,
                size: key.data_size(),
                mtime: nt_time(key.modification_time().nt_timestamp()),
                inside_image: true,
            });
        }

        // Junctions such as "Documents and Settings" show as links to their target.
        for (position, record) in links {
            let file = self.ntfs.file(fs, record)?;
            if let Some(target) = reparse_data(&file, fs)?.as_deref().and_then(link_target) {
                entries[position].kind = EntryKind::Symlink {
                    target: PathBuf::from(target),
                };
            }
        }
        Ok(entries)
    }

    /// Writes the contents of the file at `path` to `out`, decompressing them if needed.
    ///
    /// `shown` is the path to name in errors.
    pub fn write_file<R: Read + Seek>(
        &self,
        fs: &mut R,
        path: &Path,
        shown: &Path,
        out: &mut dyn Write,
    ) -> Result<()> {
        let file = self.file(fs, path)?;
        let unsupported = |reason: String| Error::UnsupportedFile {
            path: shown.to_path_buf(),
            reason,
        };

        if let Some(data) = reparse_data(&file, fs)? {
            match u32::from_le_bytes(data[..4].try_into().unwrap()) {
                wof::REPARSE_TAG => {
                    let format = wof::parse_reparse_point(&data).map_err(unsupported)?;
                    return self.write_wof(&file, fs, format, out);
                }
                REPARSE_TAG_MOUNT_POINT | REPARSE_TAG_SYMLINK => {
                    let target = link_target(&data).unwrap_or_default();
                    return Err(unsupported(format!("It is a link to “{target}”")));
                }
                REPARSE_TAG_DEDUP => {
                    return Err(unsupported(
                        "Its data is in the store of Windows Server deduplication".into(),
                    ));
                }
                // Other reparse points, e.g. of OneDrive, keep their data in the file.
                _ => {}
            }
        }
        // Checked after links, which can be folders too.
        if file.is_directory() {
            return Err(Error::NotAFile(shown.to_path_buf()));
        }

        let Some(item) = file.data(fs, "") else {
            return Ok(());
        };
        let item = item?;
        let attribute = item.to_attribute()?;
        let flags = attribute.flags();
        if flags.contains(NtfsAttributeFlags::ENCRYPTED) {
            return Err(unsupported("It is encrypted with EFS".into()));
        }
        let value = attribute.value(fs)?;
        // Resident data is never compressed, whatever the flag says.
        if flags.contains(NtfsAttributeFlags::COMPRESSED) && !attribute.is_resident() {
            let cluster_size = u64::from(self.ntfs.cluster_size());
            if cluster_size > 4096 {
                return Err(unsupported(
                    "NTFS compression with clusters over 4 KiB is not supported".into(),
                ));
            }
            let size = value.len();
            let clusters = NtfsClusters { value, fs };
            io::copy(&mut Lznt1Reader::new(clusters, cluster_size, size), out)?;
        } else {
            io::copy(&mut value.attach(fs), out)?;
        }
        Ok(())
    }

    fn write_wof<R: Read + Seek>(
        &self,
        file: &NtfsFile<'_>,
        fs: &mut R,
        format: wof::Format,
        out: &mut dyn Write,
    ) -> Result<()> {
        // The unnamed stream is a sparse placeholder of the uncompressed size.
        let size = match file.data(fs, "") {
            Some(item) => item?.to_attribute()?.value_length(),
            None => 0,
        };
        let item = file
            .data(fs, wof::STREAM_NAME)
            .ok_or_else(|| Error::Image("WOF-compressed file without its data".into()))??;
        let attribute = item.to_attribute()?;
        let stream_len = attribute.value_length();
        let stream = attribute.value(fs)?.attach(fs);
        io::copy(&mut WofReader::new(stream, stream_len, format, size), out)?;
        Ok(())
    }
}

/// The clusters of a non-resident attribute, located through its data runs.
struct NtfsClusters<'n, 'f, 'a, R> {
    value: NtfsAttributeValue<'n, 'f>,
    fs: &'a mut R,
}

impl<R: Read + Seek> Clusters for NtfsClusters<'_, '_, '_, R> {
    fn position(&mut self, offset: u64) -> io::Result<Option<u64>> {
        // Seeking follows the data runs, also past the used size up to the allocated one.
        self.value
            .seek(self.fs, SeekFrom::Start(offset))
            .map_err(io_error)?;
        Ok(self.value.data_position().value().map(|p| p.get()))
    }

    fn read_at(&mut self, position: u64, buf: &mut [u8]) -> io::Result<()> {
        self.fs.seek(SeekFrom::Start(position))?;
        self.fs.read_exact(buf)
    }
}

/// The reparse point of `file`, starting with its tag, if it has one.
fn reparse_data<R: Read + Seek>(file: &NtfsFile<'_>, fs: &mut R) -> Result<Option<Vec<u8>>> {
    let mut attributes = file.attributes();
    while let Some(item) = attributes.next(fs) {
        let item = item?;
        let attribute = item.to_attribute()?;
        if attribute.ty().ok() != Some(NtfsAttributeType::ReparsePoint) {
            continue;
        }
        // Reparse points are at most 16 KiB.
        let len = attribute.value_length().min(16 * 1024) as usize;
        if len < 8 {
            return Ok(None);
        }
        let mut data = vec![0; len];
        attribute.value(fs)?.read_exact(fs, &mut data)?;
        return Ok(Some(data));
    }
    Ok(None)
}

/// The target of a junction or symbolic link reparse point.
fn link_target(data: &[u8]) -> Option<String> {
    let tag = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    // Names follow the offsets, and for symbolic links a flags field.
    let names = match tag {
        REPARSE_TAG_MOUNT_POINT => 16,
        REPARSE_TAG_SYMLINK => 20,
        _ => return None,
    };
    let u16_at = |offset: usize| {
        data.get(offset..offset + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let name = |offset: usize| -> Option<String> {
        let start = names + u16_at(offset)?;
        let bytes = data.get(start..start + u16_at(offset + 2)?)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        Some(String::from_utf16_lossy(&units)).filter(|n| !n.is_empty())
    };
    // The print name is meant for display; the substitute name has an NT prefix.
    name(12).or_else(|| name(8).map(|n| n.trim_start_matches(r"\??\").to_owned()))
}

/// Converts an NTFS time (100 ns since 1601) to a timestamp.
fn nt_time(time: u64) -> Option<Timestamp> {
    const UNIX_EPOCH: i128 = 116_444_736_000_000_000;
    if time == 0 {
        return None;
    }
    Timestamp::from_nanosecond((i128::from(time) - UNIX_EPOCH) * 100).ok()
}

fn io_error(err: NtfsError) -> io::Error {
    match err {
        NtfsError::Io(err) => err,
        err => io::Error::new(io::ErrorKind::InvalidData, err.to_string()),
    }
}

impl From<NtfsError> for Error {
    fn from(err: NtfsError) -> Self {
        match err {
            NtfsError::Io(err) => Self::Io(err),
            err => Self::Image(format!("NTFS: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reparse_point(tag: u32, names: usize, substitute: &str, print: &str) -> Vec<u8> {
        let utf16 = |s: &str| {
            s.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>()
        };
        let (substitute, print) = (utf16(substitute), utf16(print));
        let mut data = tag.to_le_bytes().to_vec();
        data.extend([0; 4]);
        for value in [0, substitute.len(), substitute.len(), print.len()] {
            data.extend((value as u16).to_le_bytes());
        }
        data.resize(names, 0);
        data.extend(substitute);
        data.extend(print);
        data
    }

    #[test]
    fn reads_link_targets() {
        let junction = reparse_point(REPARSE_TAG_MOUNT_POINT, 16, r"\??\C:\Users", r"C:\Users");
        assert_eq!(link_target(&junction).as_deref(), Some(r"C:\Users"));

        let symlink = reparse_point(REPARSE_TAG_SYMLINK, 20, r"\??\C:\Data", "");
        assert_eq!(link_target(&symlink).as_deref(), Some(r"C:\Data"));

        assert_eq!(link_target(&wof::REPARSE_TAG.to_le_bytes()), None);
        assert_eq!(link_target(&junction[..20]), None);
    }

    #[test]
    fn converts_nt_times() {
        assert_eq!(nt_time(0), None);
        assert_eq!(
            nt_time(116_444_736_000_000_000 + 10_000_000),
            Some(Timestamp::from_second(1).unwrap())
        );
    }
}
