//! Partition tables and filesystem detection.

use std::io::{self, Read, Seek, SeekFrom};

/// What a partition contains, detected from its first sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Ntfs,
    Fat,
    BitLocker,
    Luks,
    Ext,
    Lvm,
    ExFat,
    Refs,
    Unknown,
}

impl FsKind {
    /// Why the partition can't be browsed, for kinds that can't.
    pub fn unsupported_reason(self) -> Option<&'static str> {
        Some(match self {
            Self::Ntfs | Self::Fat => return None,
            Self::BitLocker => "Encrypted with BitLocker, which can't be browsed yet",
            Self::Luks => "Encrypted with LUKS, which can't be browsed yet",
            Self::Ext => "ext2/3/4 filesystems can't be browsed yet",
            Self::Lvm => "LVM volumes can't be browsed yet",
            Self::ExFat => "exFAT filesystems can't be browsed yet",
            Self::Refs => "ReFS filesystems can't be browsed",
            Self::Unknown => "No supported filesystem",
        })
    }
}

/// A partition of a disk image, or the whole image if it has no partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// As in the partition table, starting at 1.
    pub number: u32,
    pub start: u64,
    pub len: u64,
    pub fs: FsKind,
    /// A short description such as "EFI", from the partition type.
    pub role: Option<&'static str>,
}

/// GPT partition types with a well-known role, as on-disk GUIDs.
const GPT_ROLES: [(&str, &str); 4] = [
    ("c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "EFI"),
    ("e3c9e316-0b5c-4db8-817d-f92df00215ae", "Reserved"),
    ("de94bba4-06d1-4d40-a16a-bfd50179d6ac", "Recovery"),
    ("21686148-6449-6e6f-744e-656564454649", "BIOS boot"),
];

/// Reads the partitions of the disk image `disk`, which is `len` bytes long.
pub fn read_partitions<R: Read + Seek>(disk: &mut R, len: u64) -> io::Result<Vec<Partition>> {
    // A filesystem right at the start: an image of a single partition.
    let fs = detect_fs(disk, 0)?;
    if fs != FsKind::Unknown {
        return Ok(vec![Partition {
            number: 1,
            start: 0,
            len,
            fs,
            role: None,
        }]);
    }

    let mut found = Vec::new();
    if let Ok(gpt) = gptman::GPT::find_from(disk) {
        for (number, entry) in gpt.iter().filter(|(_, e)| e.is_used()) {
            let Ok(sectors) = entry.size() else { continue };
            found.push(Partition {
                number,
                start: entry.starting_lba * gpt.sector_size,
                len: sectors * gpt.sector_size,
                fs: FsKind::Unknown,
                role: gpt_role(&entry.partition_type_guid),
            });
        }
    } else if let Ok(mbr) = mbrman::MBR::read_from(disk, 512) {
        for (number, entry) in mbr.iter() {
            if !entry.is_used() || entry.is_extended() {
                continue;
            }
            found.push(Partition {
                number: number as u32,
                start: u64::from(entry.starting_lba) * 512,
                len: u64::from(entry.sectors) * 512,
                fs: FsKind::Unknown,
                role: (entry.sys == 0xEF).then_some("EFI"),
            });
        }
    }

    // Garbage that happens to look like a table must not send reads past the image.
    found.retain(|p| p.len > 0 && p.start.checked_add(p.len).is_some_and(|end| end <= len));
    for partition in &mut found {
        partition.fs = detect_fs(disk, partition.start)?;
    }
    if found.is_empty() {
        found.push(Partition {
            number: 1,
            start: 0,
            len,
            fs: FsKind::Unknown,
            role: None,
        });
    }
    Ok(found)
}

fn gpt_role(guid: &[u8; 16]) -> Option<&'static str> {
    GPT_ROLES
        .iter()
        .find(|(text, _)| guid_bytes(text) == *guid)
        .map(|(_, role)| *role)
}

/// The on-disk form of a GUID: the first three groups little-endian.
fn guid_bytes(text: &str) -> [u8; 16] {
    let hex: Vec<u8> = text
        .split('-')
        .flat_map(|group| {
            (0..group.len())
                .step_by(2)
                .map(move |i| u8::from_str_radix(&group[i..i + 2], 16).unwrap())
        })
        .collect();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hex);
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    bytes
}

/// Detects the filesystem from the boot sector and superblocks at `start`.
fn detect_fs<R: Read + Seek>(disk: &mut R, start: u64) -> io::Result<FsKind> {
    let mut head = vec![0; 4096];
    disk.seek(SeekFrom::Start(start))?;
    let mut filled = 0;
    while filled < head.len() {
        match disk.read(&mut head[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    head.truncate(filled);
    Ok(detect_fs_in(&head))
}

fn detect_fs_in(head: &[u8]) -> FsKind {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    let boot_signature = at(510, &[0x55, 0xAA]);
    if at(3, b"NTFS    ") {
        FsKind::Ntfs
    } else if at(3, b"-FVE-FS-") {
        FsKind::BitLocker
    } else if at(3, b"EXFAT   ") {
        FsKind::ExFat
    } else if at(3, b"ReFS") {
        FsKind::Refs
    } else if boot_signature && (at(0x52, b"FAT32   ") || at(0x36, b"FAT1") || at(0x36, b"FAT ")) {
        FsKind::Fat
    } else if at(0, b"LUKS\xba\xbe") {
        FsKind::Luks
    } else if at(512, b"LABELONE") {
        FsKind::Lvm
    } else if at(0x438, &[0x53, 0xEF]) {
        FsKind::Ext
    } else {
        FsKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_are_mixed_endian_on_disk() {
        assert_eq!(
            guid_bytes("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"),
            [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
                0xc9, 0x3b
            ]
        );
    }

    #[test]
    fn detects_filesystems_from_their_signatures() {
        let with = |offset: usize, magic: &[u8]| {
            let mut head = vec![0; 4096];
            head[offset..offset + magic.len()].copy_from_slice(magic);
            head
        };
        assert_eq!(detect_fs_in(&with(3, b"NTFS    ")), FsKind::Ntfs);
        assert_eq!(detect_fs_in(&with(3, b"-FVE-FS-")), FsKind::BitLocker);
        assert_eq!(detect_fs_in(&with(0, b"LUKS\xba\xbe")), FsKind::Luks);
        assert_eq!(detect_fs_in(&with(512, b"LABELONE")), FsKind::Lvm);
        assert_eq!(detect_fs_in(&with(0x438, &[0x53, 0xEF])), FsKind::Ext);
        let mut fat = with(0x52, b"FAT32   ");
        fat[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert_eq!(detect_fs_in(&fat), FsKind::Fat);
        // Without the boot signature, a stray "FAT32" string is no filesystem.
        assert_eq!(detect_fs_in(&with(0x52, b"FAT32   ")), FsKind::Unknown);
        assert_eq!(detect_fs_in(&[0; 100]), FsKind::Unknown);
    }
}
