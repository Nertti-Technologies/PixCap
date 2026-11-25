//! File writes that never leave a half-written file behind.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to `path` via a temporary file in the same directory, then renames it.
///
/// A crash or full disk part-way through leaves the previous file (or none)
/// rather than a truncated one, which a reader would otherwise reject as
/// corrupt (a sidecar) or show as a broken image (a render).
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    let (temp, mut file) = create_temp_for(path)?;

    let result = (|| {
        // Keep the permissions of the file being replaced (a 0600 file stays 0600).
        // Unix only: on Windows the one permission is read-only, and copying it
        // would make the temp file impossible to rename or remove.
        #[cfg(unix)]
        if let Ok(existing) = fs::metadata(path) {
            file.set_permissions(existing.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Creates the hidden sibling a write goes to first: `shot.png` -> `.shot.png.<pid>-<n>.tmp`.
///
/// The name is unique per process and call, so two writes to the same target
/// never share a temp file, and it is created exclusively (`create_new`,
/// O_EXCL), which fails rather than following a symlink planted at that name.
pub(crate) fn create_temp_for(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    // A few attempts cover a stale temp file left by a crashed process with a reused pid.
    for _ in 0..8 {
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let temp = path.with_file_name(temp_name);
        match fs::OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary file name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join("pixcap-fsutil-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.json");

        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "temp file left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_permissions_and_ignores_planted_links() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("pixcap-fsutil-perm-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("private.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        // A link at the old fixed temp name must not be written through.
        let victim = dir.join("victim");
        fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(".private.json.tmp")).unwrap();

        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_keeps_the_old_file() {
        let dir = std::env::temp_dir().join("pixcap-fsutil-fail-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Renaming a file over a directory fails, after the temp file is written.
        let target = dir.join("occupied");
        fs::create_dir_all(target.join("child")).unwrap();

        assert!(write_atomic(&target, b"data").is_err());
        assert!(target.is_dir());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "temp file left behind");
        let _ = fs::remove_dir_all(&dir);
    }
}
