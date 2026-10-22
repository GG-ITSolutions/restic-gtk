//! LVM2 volume groups: the logical volumes of a physical volume, browsed like folders.

use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path},
};

use embedded_io::ErrorKind;
use lamlvm::{LV, Lvm2, OpenLvError, OwnedLvReader};
use rustic_core::jiff::Timestamp;

use super::{
    ext::ExtVolume,
    partition::{FsKind, detect_fs},
};
use crate::backend::{Entry, EntryKind, Error, Result};

/// The parsed metadata of a volume group on one physical volume.
///
/// Holds no reader: every call gets a function that opens the physical volume anew, and builds
/// the logical volume on top of that. A logical volume is a plain `Read + Seek`, so the
/// filesystem on it is read like one on a partition.
pub struct LvmVolume {
    lvm: Lvm2,
}

/// A logical volume as `std::io` source, reading from the physical volume `R`.
type LvReader<R> = ToStd<OwnedLvReader<FromStd<R>>>;

impl LvmVolume {
    pub fn open<R: Read + Seek>(pv: R) -> Result<Self> {
        let lvm = Lvm2::open(FromStd(pv)).map_err(open_error)?;
        Ok(Self { lvm })
    }

    /// The entries of the folder at `path` below the volume group, named below `parent`.
    ///
    /// The first level are the logical volumes, below them the files of their filesystems.
    pub fn list<R: Read + Seek + 'static>(
        &self,
        open_pv: impl Fn() -> R,
        path: &Path,
        parent: &Path,
    ) -> Result<Vec<Entry>> {
        let Some((lv, rest)) = self.logical_volume(path, parent)? else {
            return self.list_volumes(open_pv, parent);
        };
        let mut reader = self.reader(lv, open_pv(), parent)?;
        match filesystem(&mut reader)? {
            FsKind::Ext => ExtVolume::list(reader, rest, parent),
            other => Err(Error::UnsupportedFilesystem(reason(other).to_owned())),
        }
    }

    /// Writes the contents of the file at `path` to `out`.
    pub fn write_file<R: Read + Seek + 'static>(
        &self,
        open_pv: impl Fn() -> R,
        path: &Path,
        shown: &Path,
        out: &mut dyn Write,
    ) -> Result<()> {
        let (lv, rest) = self
            .logical_volume(path, shown)?
            .ok_or_else(|| Error::NotAFile(shown.to_path_buf()))?;
        let mut reader = self.reader(lv, open_pv(), shown)?;
        match filesystem(&mut reader)? {
            FsKind::Ext => ExtVolume::write_file(reader, rest, shown, out),
            other => Err(Error::UnsupportedFilesystem(reason(other).to_owned())),
        }
    }

    /// One entry per visible logical volume.
    ///
    /// A volume that can't be opened or probed is unavailable; only I/O errors (e.g. the
    /// network) fail the whole listing.
    fn list_volumes<R: Read + Seek + 'static>(
        &self,
        open_pv: impl Fn() -> R,
        parent: &Path,
    ) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for lv in self.lvm.lvs().filter(is_visible) {
            let name = self.name(lv);
            let path = parent.join(&name);
            let probed = self
                .reader(lv, open_pv(), &path)
                .and_then(|mut reader| filesystem(&mut reader));
            let kind = match probed {
                Ok(FsKind::Ext) => EntryKind::Partition,
                Ok(other) => EntryKind::Unavailable {
                    reason: reason(other).to_owned(),
                },
                Err(Error::UnsupportedFilesystem(reason) | Error::Image(reason)) => {
                    EntryKind::Unavailable { reason }
                }
                Err(err) => return Err(err),
            };
            entries.push(Entry {
                path,
                name,
                kind,
                size: lv.size_in_extents().saturating_mul(self.lvm.extent_size()),
                mtime: i64::try_from(lv.raw_metadata().creation_time)
                    .ok()
                    .filter(|&seconds| seconds > 0)
                    .and_then(|seconds| Timestamp::from_second(seconds).ok()),
                inside_image: true,
            });
        }
        Ok(entries)
    }

    /// The visible logical volume named by the first component of `path`, and the rest of it.
    ///
    /// `shown` is the virtual path of `path`, named when no such volume exists.
    fn logical_volume<'p>(
        &self,
        path: &'p Path,
        shown: &Path,
    ) -> Result<Option<(LV<'_>, &'p Path)>> {
        let mut components = path.components();
        let first = loop {
            match components.next() {
                Some(Component::Normal(name)) => break name,
                Some(_) => continue,
                None => return Ok(None),
            }
        };
        let lv = self
            .lvm
            .lvs()
            .filter(is_visible)
            .find(|lv| first.to_str() == Some(self.name(*lv).as_str()))
            .ok_or_else(|| Error::NotFound(shown.to_path_buf()))?;
        Ok(Some((lv, components.as_path())))
    }

    /// The name shown for `lv`: `<vg>-<lv>`, e.g. "vg0-root". Unlike the device mapper, hyphens
    /// inside the names are not doubled (`my-vg` + `root` is "my-vg-root", not "my--vg-root").
    fn name(&self, lv: LV<'_>) -> String {
        format!("{}-{}", self.lvm.vg_name(), lv.name())
    }

    /// `lv` as reader over `pv`; `UnsupportedFilesystem` for types of logical volume that can't be read.
    ///
    /// `shown` is the virtual path to name in errors.
    fn reader<R: Read + Seek>(&self, lv: LV<'_>, pv: R, shown: &Path) -> Result<LvReader<R>> {
        match self.lvm.open_lv_owned_by_name(lv.name(), FromStd(pv)) {
            Ok(Some(reader)) => Ok(ToStd(reader)),
            Ok(None) => Err(Error::NotFound(shown.to_path_buf())),
            Err(OpenLvError::Unsupported(detail)) => {
                Err(Error::UnsupportedFilesystem(unsupported_volume(lv, detail)))
            }
            Err(err) => Err(Error::Image(format!("The LVM volume is damaged: {err:?}"))),
        }
    }
}

fn open_error(err: lamlvm::Error) -> Error {
    const DAMAGED: &str = "The LVM metadata is damaged or unsupported";
    match err {
        lamlvm::Error::Io { kind } => Error::Io(io::Error::new(kind.into(), err.to_string())),
        // nom's parse errors dump the whole input buffer; a short reason is worth showing.
        lamlvm::Error::Parse { reason } if reason.len() > 80 => Error::Image(DAMAGED.to_owned()),
        err => Error::Image(format!("{DAMAGED}: {err}")),
    }
}

/// Hidden volumes such as the parts of a thin pool have no VISIBLE flag.
fn is_visible(lv: &LV<'_>) -> bool {
    lv.raw_metadata()
        .status
        .iter()
        .any(|flag| flag == "VISIBLE")
}

fn unsupported_volume(lv: LV<'_>, detail: &str) -> String {
    let kind = lv
        .raw_metadata()
        .segments
        .0
        .values()
        .next()
        .map_or("", |segment| segment.r#type.as_str());
    match kind {
        "thin" => "Thin LVM volumes can't be browsed yet".to_owned(),
        "thin-pool" => "LVM thin pools can't be browsed".to_owned(),
        _ => format!("This kind of LVM volume can't be browsed yet ({kind}: {detail})"),
    }
}

/// Detects the filesystem at the start of a logical volume.
fn filesystem<R: Read + Seek>(volume: &mut R) -> Result<FsKind> {
    let kind = detect_fs(volume, 0)?;
    if kind == FsKind::Unknown {
        // Extents beyond the end of the image (bad metadata, truncated image) read as nothing,
        // at the start or at least at the end of the volume.
        let len = volume.seek(SeekFrom::End(0))?;
        let last = match len.checked_sub(1) {
            Some(last) => {
                volume.seek(SeekFrom::Start(last))?;
                match volume.read(&mut [0; 1]) {
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => 0,
                    read => read?,
                }
            }
            None => 0,
        };
        if last == 0 {
            return Err(Error::UnsupportedFilesystem(
                "Its data lies outside the image".to_owned(),
            ));
        }
    }
    Ok(kind)
}

/// Why the filesystem on a logical volume can't be browsed.
fn reason(fs: FsKind) -> &'static str {
    fs.unsupported_reason()
        .unwrap_or("Only ext2/3/4 filesystems can be browsed inside LVM volumes")
}

/// An I/O error that keeps the original for `embedded_io`, which only knows the kind.
#[derive(Debug)]
struct IoError(io::Error);

impl embedded_io::Error for IoError {
    fn kind(&self) -> ErrorKind {
        self.0.kind().into()
    }
}

/// `std::io` source as `embedded_io` source, for lamlvm.
struct FromStd<R>(R);

impl<R> embedded_io::ErrorType for FromStd<R> {
    type Error = IoError;
}

impl<R: Read> embedded_io::Read for FromStd<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, IoError> {
        self.0.read(buf).map_err(IoError)
    }
}

impl<R: Seek> embedded_io::Seek for FromStd<R> {
    fn seek(&mut self, pos: embedded_io::SeekFrom) -> std::result::Result<u64, IoError> {
        self.0.seek(pos.into()).map_err(IoError)
    }
}

/// `embedded_io` source as `std::io` source, for the logical volume.
struct ToStd<T>(T);

impl<T> Read for ToStd<T>
where
    T: embedded_io::Read<Error = OpenLvError<IoError>>,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(lv_error)
    }
}

impl<T> Seek for ToStd<T>
where
    T: embedded_io::Seek<Error = OpenLvError<IoError>>,
{
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos.into()).map_err(lv_error)
    }
}

fn lv_error(err: OpenLvError<IoError>) -> io::Error {
    match err {
        OpenLvError::Pv(IoError(err)) => err,
        OpenLvError::Unsupported(detail) => io::Error::new(io::ErrorKind::Unsupported, detail),
        OpenLvError::NoSegment => io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "The LVM volume has no data at this position",
        ),
        OpenLvError::SeekOverflow => io::Error::new(io::ErrorKind::InvalidInput, "Seek overflow"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use embedded_io::{Read as _, Seek as _};

    use super::*;

    #[test]
    fn std_sources_work_as_embedded_io() {
        let mut source = FromStd(Cursor::new(vec![1, 2, 3, 4]));
        source.seek(embedded_io::SeekFrom::Start(2)).unwrap();
        let mut buf = [0; 4];
        assert_eq!(source.read(&mut buf).unwrap(), 2);
        assert_eq!(buf[..2], [3, 4]);
    }

    #[test]
    fn volume_errors_become_io_errors() {
        let err = lv_error(OpenLvError::Pv(IoError(io::Error::from(
            io::ErrorKind::TimedOut,
        ))));
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let err = lv_error(OpenLvError::Unsupported("segment is not linear"));
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn the_volume_is_shared_between_threads() {
        // `Volume` lives in an `Arc` that worker threads share.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LvmVolume>();
    }

    #[test]
    fn open_rejects_other_data() {
        let result = LvmVolume::open(Cursor::new(vec![0; 4096]));
        assert!(matches!(result, Err(Error::Image(_))));
    }

    #[test]
    fn long_parse_errors_are_not_shown() {
        let message = |reason: &str| match open_error(lamlvm::Error::Parse {
            reason: reason.to_owned(),
        }) {
            Error::Image(message) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message("metadata text checksum doesn't match"),
            "The LVM metadata is damaged or unsupported: parse error: metadata text checksum doesn't match"
        );
        assert_eq!(
            message(&format!("Parsing Error: Error {{ input: {:?}", [0; 200])),
            "The LVM metadata is damaged or unsupported"
        );
    }

    #[test]
    fn volumes_without_data_are_outside_the_image() {
        assert!(matches!(
            filesystem(&mut Cursor::new(Vec::new())),
            Err(Error::UnsupportedFilesystem(reason)) if reason == "Its data lies outside the image"
        ));
        // Zeros that can be read to the end are an empty volume, not a missing one.
        assert!(matches!(
            filesystem(&mut Cursor::new(vec![0; 4096])),
            Ok(FsKind::Unknown)
        ));
    }
}
