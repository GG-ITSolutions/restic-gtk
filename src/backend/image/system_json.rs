//! Drive letters from the machine description that `backup-windows` stores next to its images.

use std::{ffi::OsStr, io::Read, sync::Arc};

use rustic_core::repofile::Node;
use serde::Deserialize;

use super::reader::SnapshotReader;
use crate::backend::session::IndexedRepo;

pub const FILE_NAME: &str = "system.json";

/// Larger files are no machine description.
const MAX_SIZE: u64 = 1 << 20;

#[derive(Deserialize)]
struct System {
    #[serde(default)]
    disks: Vec<Disk>,
}

#[derive(Deserialize)]
struct Disk {
    image: Option<String>,
    #[serde(default)]
    partitions: Vec<DiskPartition>,
}

#[derive(Deserialize)]
struct DiskPartition {
    offset: u64,
    drive_letter: Option<String>,
}

/// The drive letters of the partitions of `image`, by partition offset.
///
/// Best effort: anything unexpected just means no drive letters.
pub fn drive_letters(
    repo: &Arc<IndexedRepo>,
    node: &Node,
    image: &OsStr,
) -> Option<Vec<(u64, char)>> {
    if node.meta.size > MAX_SIZE {
        return None;
    }
    let mut data = Vec::new();
    SnapshotReader::new(repo.clone(), node)
        .ok()?
        .read_to_end(&mut data)
        .ok()?;
    parse(&data, image.to_str()?)
}

fn parse(data: &[u8], image: &str) -> Option<Vec<(u64, char)>> {
    let system: System = serde_json::from_slice(data).ok()?;
    let disk = system
        .disks
        .into_iter()
        .find(|d| d.image.as_deref() == Some(image))?;
    Some(
        disk.partitions
            .into_iter()
            .filter_map(|p| {
                let letter = p.drive_letter?.chars().next()?;
                letter
                    .is_ascii_alphabetic()
                    .then(|| (p.offset, letter.to_ascii_uppercase()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_drive_letters_of_the_image() {
        let json = br#"{
            "format": 1,
            "disks": [
                {"image": "disk1.img", "partitions": [{"offset": 1048576, "drive_letter": "D"}]},
                {"image": "disk0.img", "partitions": [
                    {"number": 1, "offset": 1048576, "drive_letter": null},
                    {"number": 3, "offset": 122683392, "drive_letter": "C"}
                ]}
            ]
        }"#;
        assert_eq!(parse(json, "disk0.img"), Some(vec![(122683392, 'C')]));
        assert_eq!(parse(json, "disk2.img"), None);
        assert_eq!(parse(b"not json", "disk0.img"), None);
    }
}
