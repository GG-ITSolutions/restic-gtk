//! LVM2 volume groups: the logical volumes of a physical volume, browsed like folders.

use std::io::{self, Read, Seek, SeekFrom};

use embedded_io::ErrorKind;
use lamlvm::{LV, Lvm2, OpenLvError, OwnedLvReader};
use rustic_core::jiff::Timestamp;

use super::{
    partition::{FsKind, detect_fs},
    reader::Shared,
};
use crate::backend::{Error, Result};

/// A visible logical volume of the volume group on a physical volume.
pub struct LogicalVolume {
    /// `<vg>-<lv>`, e.g. "vg0-root". Unlike the device mapper, hyphens inside the names are not
    /// doubled (`my-vg` + `root` is "my-vg-root", not "my--vg-root").
    pub name: String,
    pub size: u64,
    pub mtime: Option<Timestamp>,
    /// The volume as block source and the filesystem at its start, or why it can't be read.
    pub content: std::result::Result<(Shared, FsKind), String>,
}

/// A logical volume as `std::io` source, reading from the physical volume `R`.
type LvReader<R> = ToStd<OwnedLvReader<FromStd<R>>>;

/// The visible logical volumes on the physical volume `pv`.
///
/// Only I/O errors (e.g. the network) and damaged metadata fail; a volume that can't be opened
/// or probed says why in its `content`. The volumes read from clones of `pv`.
pub fn logical_volumes<R>(pv: R) -> Result<Vec<LogicalVolume>>
where
    R: Read + Seek + Send + Clone + 'static,
{
    let lvm = Lvm2::open(FromStd(pv.clone())).map_err(open_error)?;
    let mut volumes = Vec::new();
    for lv in lvm.lvs().filter(is_visible) {
        let content = match open_volume(&lvm, lv, pv.clone()) {
            Ok(content) => Ok(content),
            Err(Error::UnsupportedFilesystem(reason) | Error::Image(reason)) => Err(reason),
            Err(err) => return Err(err),
        };
        volumes.push(LogicalVolume {
            name: format!("{}-{}", lvm.vg_name(), lv.name()),
            size: lv.size_in_extents().saturating_mul(lvm.extent_size()),
            mtime: i64::try_from(lv.raw_metadata().creation_time)
                .ok()
                .filter(|&seconds| seconds > 0)
                .and_then(|seconds| Timestamp::from_second(seconds).ok()),
            content,
        });
    }
    Ok(volumes)
}

/// `lv` over `pv` as block source, with the filesystem at its start.
///
/// `UnsupportedFilesystem` for types of logical volume that can't be read.
fn open_volume<R>(lvm: &Lvm2, lv: LV<'_>, pv: R) -> Result<(Shared, FsKind)>
where
    R: Read + Seek + Send + 'static,
{
    let mut reader: LvReader<R> = match lvm.open_lv_owned_by_name(lv.name(), FromStd(pv)) {
        Ok(Some(reader)) => ToStd(reader),
        Ok(None) => {
            return Err(Error::Image(
                "The LVM volume is not in the metadata".to_owned(),
            ));
        }
        Err(OpenLvError::Unsupported(detail)) => {
            return Err(Error::UnsupportedFilesystem(unsupported_volume(lv, detail)));
        }
        Err(err) => return Err(Error::Image(format!("The LVM volume is damaged: {err:?}"))),
    };
    let fs = filesystem(&mut reader)?;
    let size = reader.seek(SeekFrom::End(0))?;
    Ok((Shared::new(reader, size), fs))
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
    fn the_volumes_are_shared_between_threads() {
        // They live in an `Arc` that worker threads share.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LogicalVolume>();
    }

    #[test]
    fn open_rejects_other_data() {
        let result = logical_volumes(Cursor::new(vec![0; 4096]));
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
