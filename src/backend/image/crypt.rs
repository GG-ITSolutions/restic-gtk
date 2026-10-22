//! Encrypted volumes: BitLocker and LUKS, unlocked into a plain `Read + Seek` source.
//!
//! The decryption is done by the `bitlocker-core` and `luks-core` crates. This module only
//! makes their readers clonable like a [`Slice`](super::reader::Slice) and turns their errors
//! into [`Error`]s, so that the crates can be replaced without touching anything else. What the
//! plaintext holds (NTFS, FAT, later ext4 or LVM) is for the caller to detect.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};

use bitlocker::{BdeError, BitLockerVolume};
use luks::{LuksError, LuksVolume};
use zeroize::Zeroizing;

use super::{partition::FsKind, wof::seek_position};
use crate::backend::{Error, Result};

/// Tells an encrypted volume apart across snapshots: the BitLocker volume GUID or the LUKS UUID.
pub(super) type VolumeId = String;

/// A recovery password or passphrase, wiped from memory when dropped.
pub(super) type Secret = Zeroizing<String>;

/// Offset and length of the UUID in the header of LUKS1 and LUKS2.
const LUKS_UUID: std::ops::Range<usize> = 168..208;

trait Plaintext: Read + Seek + Send {}
impl<T: Read + Seek + Send> Plaintext for T {}

/// The plaintext of an unlocked volume.
///
/// The decrypting readers of the crates need `&mut self` for everything, so they sit behind a
/// lock. Clones share it, but keep their own position, like clones of a `SnapshotReader`.
#[derive(Clone)]
pub(super) struct Decrypted {
    source: Arc<Mutex<Box<dyn Plaintext>>>,
    size: u64,
    pos: u64,
}

impl std::fmt::Debug for Decrypted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decrypted")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Decrypted {
    fn new(source: impl Read + Seek + Send + 'static, size: u64) -> Self {
        Self {
            source: Arc::new(Mutex::new(Box::new(source))),
            size,
            pos: 0,
        }
    }

    /// Another reader of the same plaintext, positioned at the start.
    pub fn rewound(&self) -> Self {
        Self {
            pos: 0,
            ..self.clone()
        }
    }
}

impl Read for Decrypted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(self.size.saturating_sub(self.pos) as usize);
        if len == 0 {
            return Ok(0);
        }
        let mut source = self.source.lock().unwrap();
        source.seek(SeekFrom::Start(self.pos))?;
        let n = source.read(&mut buf[..len])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Decrypted {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.size, pos)?;
        Ok(self.pos)
    }
}

/// Unlocks the encrypted volume `raw` of the kind `kind` (`BitLocker` or `Luks`).
///
/// For BitLocker, a 48 digit number is tried as recovery password first, then as password of the
/// user (one more 0.1 s stretch for a wrong recovery password, or if the volume has no recovery
/// password protector at all); anything else only as password.
/// Key derivation takes from 0.1 s (BitLocker) to several seconds (LUKS2 with Argon2), so call
/// this off the main thread.
pub(super) fn unlock<R>(kind: FsKind, raw: R, secret: &str) -> Result<Decrypted>
where
    R: Read + Seek + Send + Clone + 'static,
{
    match kind {
        FsKind::BitLocker => {
            let with_password =
                |raw| BitLockerVolume::unlock_with_password(raw, secret).map_err(bitlocker_error);
            let volume = match recovery_password(secret) {
                Some(recovery) => {
                    let again = raw.clone();
                    match BitLockerVolume::unlock_with_recovery_password(raw, &recovery)
                        .map_err(bitlocker_error)
                    {
                        Err(Error::WrongPassword | Error::UnsupportedFilesystem(_)) => {
                            with_password(again)
                        }
                        result => result,
                    }
                }
                None => with_password(raw),
            }?;
            let size = volume.volume_size();
            Ok(Decrypted::new(volume, size))
        }
        FsKind::Luks => {
            let volume =
                LuksVolume::unlock_with_passphrase(raw, secret.as_bytes()).map_err(luks_error)?;
            let size = volume.payload_size();
            Ok(Decrypted::new(volume, size))
        }
        other => Err(Error::Image(format!(
            "{other:?} is not an encrypted volume"
        ))),
    }
}

/// The identity of the encrypted volume `raw`, read from its header without unlocking it.
pub(super) fn volume_id<R: Read + Seek>(kind: FsKind, raw: &mut R) -> Result<VolumeId> {
    match kind {
        FsKind::BitLocker => {
            let metadata = BitLockerVolume::read_metadata(raw).map_err(bitlocker_error)?;
            Ok(format!(
                "bitlocker:{}",
                bitlocker::format_guid(&metadata.volume_guid)
            ))
        }
        FsKind::Luks => {
            let mut header = [0; LUKS_UUID.end];
            raw.seek(SeekFrom::Start(0))?;
            raw.read_exact(&mut header)?;
            let uuid = &header[LUKS_UUID];
            let uuid = &uuid[..uuid.iter().position(|&b| b == 0).unwrap_or(uuid.len())];
            Ok(format!("luks:{}", String::from_utf8_lossy(uuid)))
        }
        other => Err(Error::Image(format!(
            "{other:?} is not an encrypted volume"
        ))),
    }
}

/// `secret` as the dashed recovery password the crate wants, if it is one: 48 digits, which
/// may be grouped by dashes or spaces.
fn recovery_password(secret: &str) -> Option<Secret> {
    let grouped = secret
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c.is_whitespace());
    let digits: Vec<char> = secret.chars().filter(char::is_ascii_digit).collect();
    if !grouped || digits.len() != 48 {
        return None;
    }
    let groups: Vec<String> = digits.chunks(6).map(|g| g.iter().collect()).collect();
    Some(Secret::new(groups.join("-")))
}

/// Type code of the recovery password protector.
const RECOVERY_PROTECTOR: u16 = 0x0800;

/// Turns a `bitlocker-core` error into an [`Error`].
///
/// A volume without a password protector but with a recovery password (Windows device
/// encryption: TPM and recovery password) is unlockable, so anything that is not a valid 48
/// digit recovery password is a wrong key there, not an unsupported volume.
fn bitlocker_error(err: BdeError) -> Error {
    match err {
        // A wrong key makes the AES-CCM tag of the volume master key fail; a recovery password
        // with a typo already fails its checksum.
        BdeError::AuthenticationFailed { .. } | BdeError::InvalidRecoveryPassword { .. } => {
            Error::WrongPassword
        }
        BdeError::NoUnlockProtector { found, .. } if found.contains(&RECOVERY_PROTECTOR) => {
            Error::WrongPassword
        }
        BdeError::NoUnlockProtector { protector, found } => Error::UnsupportedFilesystem(format!(
            "This BitLocker volume has no {protector} protector, only {}",
            protector_names(&found)
        )),
        BdeError::UnsupportedEncryptionMethod { method }
        | BdeError::UnvalidatedEncryptionMethod { method } => Error::UnsupportedFilesystem(
            format!("BitLocker encryption method 0x{method:04x} is not supported"),
        ),
        BdeError::Io(err) => Error::Io(err),
        err => Error::Image(format!("Damaged BitLocker volume: {err}")),
    }
}

/// The key protectors of a BitLocker volume, from their type codes: “the TPM, a startup key
/// and a password”. Each kind is named once, however many protectors of it there are.
fn protector_names(found: &[u16]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for protection in found {
        let name = match *protection {
            0x0000 => "a clear key",
            0x0100 => "the TPM",
            0x0200 => "a startup key",
            0x0500 => "the TPM with a PIN",
            RECOVERY_PROTECTOR => "a recovery password",
            0x2000 => "a password",
            _ => "an unknown protector",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    match names.split_last() {
        None => "none".to_owned(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

fn luks_error(err: LuksError) -> Error {
    match err {
        LuksError::AuthenticationFailed => Error::WrongPassword,
        LuksError::Io(err) => Error::Io(err),
        LuksError::Unsupported { what, value } => {
            Error::UnsupportedFilesystem(format!("LUKS {what} “{value}” is not supported"))
        }
        LuksError::UnsupportedVersion { version } => {
            Error::UnsupportedFilesystem(format!("LUKS version {version} is not supported"))
        }
        // The cost parameters come from the header; refusing is better than guessing.
        LuksError::DerivationBudgetExceeded { .. }
        | LuksError::UnlockBudgetExceeded { .. }
        | LuksError::DerivationMemoryExceeded { .. }
        | LuksError::KeyLengthExceeded { .. } => Error::UnsupportedFilesystem(
            "The key derivation of this LUKS volume is too expensive to run here".to_owned(),
        ),
        err => Error::Image(format!("Damaged LUKS volume: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use super::*;
    use crate::backend::image::partition::detect_fs;

    const LUKS_PASSPHRASE: &str = "luks fixture";
    const BITLOCKER_PASSWORD: &str = "bitlocker fixture";
    const BITLOCKER_RECOVERY: &str = "013574-624679-047531-660000-008547-345565-299002-046662";

    /// A fixture of `tests/fixtures/generate-encrypted.sh`.
    fn fixture(name: &str) -> Cursor<Vec<u8>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{name}.img.zst"));
        let mut image = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(fs::File::open(path).unwrap())
            .unwrap()
            .read_to_end(&mut image)
            .unwrap();
        Cursor::new(image)
    }

    /// The FAT volume inside: boot sector signature and its first file.
    fn assert_fat_volume(plain: &mut Decrypted) {
        assert_eq!(detect_fs(plain, 0).unwrap(), FsKind::Fat);
        let mut boot = [0; 512];
        plain.seek(SeekFrom::Start(0)).unwrap();
        plain.read_exact(&mut boot).unwrap();
        assert_eq!(&boot[510..], [0x55, 0xAA]);
    }

    #[test]
    fn unlocks_bitlocker_with_password_and_recovery_password() {
        for secret in [
            BITLOCKER_PASSWORD,
            BITLOCKER_RECOVERY,
            // The digits may be grouped differently.
            "013574 624679 047531 660000 008547 345565 299002 046662",
            "013574624679047531660000008547345565299002046662",
        ] {
            let mut plain = unlock(FsKind::BitLocker, fixture("bitlocker"), secret).unwrap();
            assert_eq!(plain.size, 1 << 20, "{secret}");
            assert_fat_volume(&mut plain);
        }
    }

    #[test]
    fn unlocks_luks1_and_luks2() {
        // The payload is what is left after the header: 2 MiB of the 4 MiB, 1 MiB of 1.25 MiB.
        for (name, size) in [("luks1", 2 << 20), ("luks2", 1 << 20)] {
            let mut plain = unlock(FsKind::Luks, fixture(name), LUKS_PASSPHRASE).unwrap();
            assert_eq!(plain.size, size, "{name}");
            assert_fat_volume(&mut plain);
        }
    }

    #[test]
    fn a_wrong_key_is_a_wrong_password() {
        // Well-formed (every group divisible by 11), but not the key of this volume: it is also
        // tried as the password, which the password protector of the fixture rejects too.
        let wrong_recovery = "013574-624679-047531-660000-008547-345565-299002-046673";
        for secret in ["", "nope", "bitlocker fixtur", wrong_recovery] {
            let result = unlock(FsKind::BitLocker, fixture("bitlocker"), secret);
            assert!(matches!(result, Err(Error::WrongPassword)), "{secret}");
        }
        // Eight groups, but not divisible by 11: a typo.
        let typo = "013575-624679-047531-660000-008547-345565-299002-046662";
        let result = unlock(FsKind::BitLocker, fixture("bitlocker"), typo);
        assert!(matches!(result, Err(Error::WrongPassword)));
        for name in ["luks1", "luks2"] {
            let result = unlock(FsKind::Luks, fixture(name), "nope");
            assert!(matches!(result, Err(Error::WrongPassword)), "{name}");
        }
    }

    #[test]
    fn something_else_than_a_volume_is_an_image_error() {
        let result = unlock(FsKind::Luks, fixture("bitlocker"), LUKS_PASSPHRASE);
        assert!(matches!(result, Err(Error::Image(_))), "{result:?}");
        let result = unlock(FsKind::BitLocker, fixture("luks2"), BITLOCKER_PASSWORD);
        assert!(matches!(result, Err(Error::Image(_))), "{result:?}");
    }

    #[test]
    fn volume_ids_come_from_the_headers() {
        let id = |kind, name| volume_id(kind, &mut fixture(name)).unwrap();
        assert_eq!(
            id(FsKind::BitLocker, "bitlocker"),
            "bitlocker:0b1f6d3a-5c1e-4f7e-9a53-2f5a4d2d7c11"
        );
        let (luks1, luks2) = (id(FsKind::Luks, "luks1"), id(FsKind::Luks, "luks2"));
        assert!(luks1.starts_with("luks:") && luks1.len() == "luks:".len() + 36);
        assert_ne!(luks1, luks2);
    }

    #[test]
    fn clones_keep_their_own_position() {
        let mut first = unlock(FsKind::Luks, fixture("luks2"), LUKS_PASSPHRASE).unwrap();
        first.seek(SeekFrom::Start(510)).unwrap();
        let mut second = first.rewound();
        let (mut a, mut b) = ([0; 2], [0; 2]);
        first.read_exact(&mut a).unwrap();
        second.seek(SeekFrom::Start(0)).unwrap();
        second.read_exact(&mut b).unwrap();
        assert_eq!(a, [0x55, 0xAA]);
        assert_eq!(b, [0xEB, 0x3C]);
        assert_eq!(first.stream_position().unwrap(), 512);
        assert_eq!(second.stream_position().unwrap(), 2);

        // Reads stop at the end of the plaintext.
        first.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(first.read(&mut [0; 16]).unwrap(), 2);
        assert_eq!(first.read(&mut [0; 16]).unwrap(), 0);
    }

    #[test]
    fn recovery_passwords_are_told_from_passwords() {
        let dashed = "013574-624679-047531-660000-008547-345565-299002-046662";
        assert_eq!(
            recovery_password(dashed).as_deref().map(String::as_str),
            Some(dashed)
        );
        assert!(recovery_password(&dashed.replace('-', " ")).is_some());
        assert!(recovery_password(&dashed.replace('-', "")).is_some());
        assert!(recovery_password("013574-624679").is_none());
        assert!(recovery_password(&format!("{dashed}1")).is_none());
        assert!(recovery_password(&dashed.replace("013574", "01357x")).is_none());
        assert!(recovery_password("hunter2").is_none());
    }

    #[test]
    fn explains_what_cannot_be_unlocked() {
        let message = |err: Error| match err {
            Error::UnsupportedFilesystem(message) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message(bitlocker_error(BdeError::NoUnlockProtector {
                protector: "password",
                found: vec![0x0100],
            })),
            "This BitLocker volume has no password protector, only the TPM"
        );
        assert_eq!(
            message(bitlocker_error(BdeError::NoUnlockProtector {
                protector: "recovery password",
                found: vec![0x0100, 0x0200, 0x2000],
            })),
            "This BitLocker volume has no recovery password protector, only the TPM, a startup \
             key and a password"
        );
        // Several protectors of one kind are named once.
        assert_eq!(
            message(bitlocker_error(BdeError::NoUnlockProtector {
                protector: "password",
                found: vec![0x0100, 0x0100, 0x0200, 0x0100],
            })),
            "This BitLocker volume has no password protector, only the TPM and a startup key"
        );
        assert_eq!(
            message(bitlocker_error(BdeError::UnvalidatedEncryptionMethod {
                method: 0x8001
            })),
            "BitLocker encryption method 0x8001 is not supported"
        );
        assert_eq!(
            message(luks_error(LuksError::Unsupported {
                what: "cipher mode",
                value: "cbc-essiv:sha256".into(),
            })),
            "LUKS cipher mode “cbc-essiv:sha256” is not supported"
        );
        assert_eq!(
            message(luks_error(LuksError::DerivationBudgetExceeded {
                iterations: 5,
                projected_secs: 99,
                budget_secs: 30,
            })),
            "The key derivation of this LUKS volume is too expensive to run here"
        );
    }

    /// On a volume with only TPM and recovery password (Windows device encryption), a mistyped
    /// recovery key (47 or 49 digits) is tried as a password, which no protector takes.
    #[test]
    fn a_missing_password_protector_is_a_wrong_key_if_a_recovery_password_exists() {
        let no_password = |found| BdeError::NoUnlockProtector {
            protector: "password",
            found,
        };
        assert!(matches!(
            bitlocker_error(no_password(vec![0x0100, 0x0800])),
            Error::WrongPassword
        ));
        assert!(matches!(
            bitlocker_error(no_password(vec![0x0800])),
            Error::WrongPassword
        ));
        // Without a recovery password nothing typed in can help.
        assert!(matches!(
            bitlocker_error(no_password(vec![0x0100])),
            Error::UnsupportedFilesystem(message) if message.contains("the TPM")
        ));
    }

    /// The BitLocker To Go image of libbde's test data, made by Windows: AES-CBC with diffuser
    /// on FAT, password "bde-TEST". Get it from
    /// <https://github.com/log2timeline/dfvfs/raw/main/test_data/bdetogo.raw> and run
    ///
    /// ```text
    /// BDETOGO=/path/to/bdetogo.raw cargo test --lib bitlocker_to_go -- --ignored
    /// ```
    #[test]
    #[ignore = "needs bdetogo.raw, see the doc comment"]
    fn unlocks_a_bitlocker_to_go_volume_made_by_windows() {
        let path = std::env::var("BDETOGO").expect("BDETOGO is not set");
        // In memory (64 MiB): `unlock` wants a `Clone` reader, which a `File` is not.
        let image = Cursor::new(fs::read(&path).unwrap());
        assert_eq!(detect_fs(&mut image.clone(), 0).unwrap(), FsKind::BitLocker);

        let mut plain = unlock(FsKind::BitLocker, image.clone(), "bde-TEST").unwrap();
        assert_fat_volume(&mut plain);
        let wrong = unlock(FsKind::BitLocker, image, "nope");
        assert!(matches!(wrong, Err(Error::WrongPassword)));
    }
}
