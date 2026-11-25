//! File writes that never leave a half-written file behind.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` via a temporary file in the same directory, then renames it.
///
/// A crash or full disk part-way through leaves the previous file (or none)
/// rather than a truncated one, which a reader would otherwise reject as
/// corrupt (a sidecar) or show as a broken image (a render).
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    let temp = temp_path_for(path)?;

    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Hidden sibling name used while a write is in progress: `shot.png` -> `.shot.png.tmp`.
fn temp_path_for(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(".tmp");
    Ok(path.with_file_name(temp_name))
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
