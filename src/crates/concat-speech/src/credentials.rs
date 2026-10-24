// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Device-local provider credentials. Only status crosses back into the UI.
//! A routing record prevents an old OS credential from resurfacing after a
//! fallback replacement or deletion. All operations run on a worker, serialized.
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use concat_host::AppDirs;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// A key whose Debug representation is always redacted.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl Secret {
    /// Validate before any storage or network operation. No error echoes input.
    pub fn new(value: String) -> Result<Self, Error> {
        let value = Zeroizing::new(value);
        let trimmed = value.trim();
        if trimmed.is_empty()
            || trimmed.len() > 4096
            || !trimmed.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Error::Invalid);
        }
        Ok(Self(Zeroizing::new(trimmed.to_owned())))
    }

    /// Expose only to a storage adapter or HTTP authorization construction.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Sanitized errors, never provider messages or credential contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Invalid key syntax.
    Invalid,
    /// The native store is absent or cannot be used on this device.
    Unavailable,
    /// Access denied or locked; do not replace it silently with a local file.
    Locked,
    /// Local storage could not be read, updated or validated.
    File,
    /// Removal from the native store could not finish. A tombstone blocks reuse.
    Cleanup,
}

/// Status contains no key, suffix, or other credential material.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// No credential configured.
    #[default]
    Missing,
    /// Stored in the operating system's credential store.
    Secure,
    /// Plaintext fallback in the app's credential file.
    LocalFile,
}

impl Status {
    /// Whether a configured credential can be used.
    pub fn configured(self) -> bool {
        self != Self::Missing
    }
}

/// The native platform adapter. Missing entries are `Ok(None)`.
pub trait SecureStore: Send + Sync {
    /// Read the OpenRouter entry.
    fn read(&self) -> Result<Option<Secret>, Error>;
    /// Replace the OpenRouter entry.
    fn write(&self, key: &Secret) -> Result<(), Error>;
    /// Delete it; an already absent entry is success.
    fn delete(&self) -> Result<(), Error>;
}

static PLATFORM: OnceLock<Arc<dyn SecureStore>> = OnceLock::new();
static OPERATIONS: Mutex<()> = Mutex::new(());

/// Install the mobile platform adapter before the editor starts.
pub fn install_store(store: Arc<dyn SecureStore>) {
    let _ = PLATFORM.set(store);
}

struct Native;

#[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
fn native_error(error: keyring::Error) -> Error {
    match error {
        keyring::Error::NoStorageAccess(_) => Error::Locked,
        keyring::Error::BadEncoding(_) | keyring::Error::Ambiguous(_) => Error::File,
        _ => Error::Unavailable,
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
impl SecureStore for Native {
    fn read(&self) -> Result<Option<Secret>, Error> {
        let entry = keyring::Entry::new("app.concat.editor.transcription", "openrouter")
            .map_err(native_error)?;
        match entry.get_password() {
            Ok(value) => Secret::new(value).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(native_error(error)),
        }
    }
    fn write(&self, key: &Secret) -> Result<(), Error> {
        keyring::Entry::new("app.concat.editor.transcription", "openrouter")
            .map_err(native_error)?
            .set_password(key.expose())
            .map_err(native_error)
    }
    fn delete(&self) -> Result<(), Error> {
        let entry = keyring::Entry::new("app.concat.editor.transcription", "openrouter")
            .map_err(native_error)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(native_error(error)),
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "ios", target_os = "linux")))]
impl SecureStore for Native {
    fn read(&self) -> Result<Option<Secret>, Error> {
        Err(Error::Unavailable)
    }
    fn write(&self, _: &Secret) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
    fn delete(&self) -> Result<(), Error> {
        Err(Error::Unavailable)
    }
}

// The only serialized key is the explicitly supported plaintext fallback.
// The serialized buffer is zeroized after an atomic write/read.
#[derive(Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "camelCase")]
enum Record {
    Native,
    Local { key: String },
    Deleted,
}

impl Drop for Record {
    fn drop(&mut self) {
        if let Self::Local { key } = self {
            use zeroize::Zeroize;
            key.zeroize();
        }
    }
}

struct Store<'a> {
    path: PathBuf,
    native: &'a dyn SecureStore,
}

impl Store<'_> {
    fn record(&self) -> Result<Option<Record>, Error> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::File),
        };
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::File)?;
        if bytes.len() > 8192 {
            return Err(Error::File);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::File)
    }

    fn put(&self, record: &Record) -> Result<(), Error> {
        let parent = self.path.parent().ok_or(Error::File)?;
        std::fs::create_dir_all(parent).map_err(|_| Error::File)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::File)?;
        }
        let bytes = Zeroizing::new(serde_json::to_vec(record).map_err(|_| Error::File)?);
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| Error::File)?;
        file.write_all(&bytes).map_err(|_| Error::File)?;
        file.as_file().sync_all().map_err(|_| Error::File)?;
        file.persist(&self.path).map_err(|_| Error::File)?;
        Ok(())
    }

    fn read(&self) -> Result<(Option<Secret>, Status), Error> {
        match self.record()? {
            Some(Record::Deleted) => Ok((None, Status::Missing)),
            Some(Record::Local { ref key }) => {
                Ok((Some(Secret::new(key.clone())?), Status::LocalFile))
            }
            Some(Record::Native) | None => match self.native.read() {
                Ok(Some(key)) => Ok((Some(key), Status::Secure)),
                Ok(None) => Ok((None, Status::Missing)),
                // No entry was saved on a system without a credential service.
                Err(Error::Unavailable) if self.record()?.is_none() => Ok((None, Status::Missing)),
                Err(error) => Err(error),
            },
        }
    }

    fn save(&self, key: &Secret) -> Result<Status, Error> {
        match self.native.write(key) {
            Ok(()) => {
                self.put(&Record::Native)?;
                Ok(Status::Secure)
            }
            Err(Error::Unavailable) => {
                self.put(&Record::Local {
                    key: key.expose().to_owned(),
                })?;
                Ok(Status::LocalFile)
            }
            Err(error) => Err(error),
        }
    }

    fn delete(&self) -> Result<Status, Error> {
        // Block reuse first, even if the OS store is locked/offline. The same
        // path atomically replaces a fallback key, so no stale key is retained.
        self.put(&Record::Deleted)?;
        self.native.delete().map_err(|_| Error::Cleanup)?;
        Ok(Status::Missing)
    }
}

fn with_store<T>(dirs: &AppDirs, run: impl FnOnce(Store<'_>) -> T) -> Result<T, Error> {
    let _guard = OPERATIONS.lock().map_err(|_| Error::Unavailable)?;
    let native = Native;
    let backend = PLATFORM
        .get()
        .map_or(&native as &dyn SecureStore, |p| p.as_ref());
    // AppDirs already selects portable/ beside the executable when requested.
    // Windows uses non-roaming local data; iOS uses durable Application Support.
    let root = if cfg!(windows) {
        &dirs.data
    } else {
        &dirs.config
    };
    Ok(run(Store {
        path: root.join("credentials/openrouter.json"),
        native: backend,
    }))
}

/// Inspect on a worker, never as part of a UI publish or animation tick.
pub fn status(dirs: &AppDirs) -> Result<Status, Error> {
    with_store(dirs, |s| s.read().map(|(_, status)| status))?
}

/// Resolve only when a paid request is about to start.
pub fn read(dirs: &AppDirs) -> Result<Secret, Error> {
    with_store(dirs, |s| {
        s.read().and_then(|(key, _)| key.ok_or(Error::Invalid))
    })?
}

/// Save to the OS store, falling back to a local file only when unavailable.
pub fn save(dirs: &AppDirs, key: &Secret) -> Result<Status, Error> {
    with_store(dirs, |s| s.save(key))?
}

/// Delete the app's active key and suppress any stale OS copy.
pub fn delete(dirs: &AppDirs) -> Result<Status, Error> {
    with_store(dirs, |s| s.delete())?
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        key: Mutex<Option<Secret>>,
        error: Mutex<Option<Error>>,
    }
    impl SecureStore for Fake {
        fn read(&self) -> Result<Option<Secret>, Error> {
            if let Some(e) = *self.error.lock().unwrap() {
                return Err(e);
            }
            Ok(self.key.lock().unwrap().clone())
        }
        fn write(&self, key: &Secret) -> Result<(), Error> {
            if let Some(e) = *self.error.lock().unwrap() {
                return Err(e);
            }
            *self.key.lock().unwrap() = Some(key.clone());
            Ok(())
        }
        fn delete(&self) -> Result<(), Error> {
            if let Some(e) = *self.error.lock().unwrap() {
                return Err(e);
            }
            *self.key.lock().unwrap() = None;
            Ok(())
        }
    }
    #[test]
    fn fallback_replacement_migration_and_delete_never_resurrect_stale_keys() {
        let dir = tempfile::tempdir().unwrap();
        let native = Fake {
            key: Mutex::new(None),
            error: Mutex::new(None),
        };
        let store = Store {
            path: dir.path().join("credentials/openrouter.json"),
            native: &native,
        };
        let first = Secret::new("first-key".into()).unwrap();
        assert_eq!(store.save(&first), Ok(Status::Secure));
        assert!(
            !std::fs::read_to_string(&store.path)
                .unwrap()
                .contains("first-key")
        );
        *native.error.lock().unwrap() = Some(Error::Unavailable);
        assert_eq!(
            store.save(&Secret::new("replacement".into()).unwrap()),
            Ok(Status::LocalFile)
        );
        assert_eq!(store.read().unwrap().0.unwrap().expose(), "replacement");
        assert_eq!(store.delete(), Err(Error::Cleanup));
        *native.error.lock().unwrap() = None;
        assert_eq!(store.read().unwrap().1, Status::Missing);
        assert_eq!(store.save(&first), Ok(Status::Secure));
        assert!(
            !std::fs::read_to_string(&store.path)
                .unwrap()
                .contains("replacement")
        );
        assert_eq!(store.delete(), Ok(Status::Missing));
    }
    #[test]
    fn denied_access_does_not_write_plaintext_and_corrupt_records_do_not_fall_through() {
        let dir = tempfile::tempdir().unwrap();
        let native = Fake {
            key: Mutex::new(None),
            error: Mutex::new(Some(Error::Locked)),
        };
        let store = Store {
            path: dir.path().join("key.json"),
            native: &native,
        };
        assert_eq!(
            store.save(&Secret::new("test-key".into()).unwrap()),
            Err(Error::Locked)
        );
        assert!(!store.path.exists());
        std::fs::write(&store.path, "corrupt").unwrap();
        assert!(matches!(store.read(), Err(Error::File)));
        assert_eq!(
            format!("{:?}", Secret::new("private-value".into()).unwrap()),
            "Secret([redacted])"
        );
        assert!(Secret::new("bad\r\nheader".into()).is_err());
    }
}
