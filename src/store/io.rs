//! Raw file IO for the store: lenient JSON reads and crash-safe writes.
//!
//! Writes go to a `<name>.tmp` sibling (covered by the store's gitignore), get fsynced, and are renamed into place, so a crash
//! mid-write can never leave a half-written board. Because replacement is a rename, *readers need no lock*: they always see
//! either the old file or the new one, never a torn mix.

use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::Path,
};

use serde::{Serialize, de::DeserializeOwned};

use super::StoreError;

/// Read and parse a JSON file. `Ok(None)` when the file doesn't exist — absence is meaningful (uninitialised board, empty
/// claims), not an error.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|source| StoreError::Parse { path: path.to_owned(), source }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io { path: path.to_owned(), source }),
    }
}

/// Serialize `value` as pretty JSON (with a trailing newline — the files are meant to be read and hand-edited) and atomically
/// replace `path` with it via a same-directory temp file and rename.
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let io_err = |source| StoreError::Io { path: path.to_owned(), source };
    let mut text = serde_json::to_string_pretty(value).map_err(|source| StoreError::Parse { path: path.to_owned(), source })?;
    text.push('\n');

    let tmp = tmp_path(path);
    let mut file = File::create(&tmp).map_err(io_err)?;
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).map_err(io_err)?;
    drop(file);
    fs::rename(&tmp, path).map_err(io_err)?;

    // Make the rename itself durable. Best-effort: some filesystems refuse directory fsync, and the write is already atomic.
    if let Some(dir) = path.parent() {
        let _ = File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// `board.json` → `board.json.tmp`, in the same directory so the rename stays on one filesystem.
fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let read: Option<Vec<u32>> = read_json(&dir.path().join("absent.json")).unwrap();
        assert!(read.is_none());
    }

    #[test]
    fn write_then_read_round_trips_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        write_json_atomic(&path, &vec![1u32, 2, 3]).unwrap();
        assert_eq!(read_json::<Vec<u32>>(&path).unwrap(), Some(vec![1, 2, 3]));
        assert!(!path.with_file_name("board.json.tmp").exists(), "tmp file must be renamed away");
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'), "hand-editable files end in a newline");
    }

    #[test]
    fn a_leftover_tmp_never_shadows_the_real_file() {
        // Simulates a crash between temp-write and rename: the target must stay intact and a later write must still succeed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        write_json_atomic(&path, &"old").unwrap();
        fs::write(path.with_file_name("board.json.tmp"), "garbage{{{").unwrap();
        assert_eq!(read_json::<String>(&path).unwrap(), Some("old".into()));
        write_json_atomic(&path, &"new").unwrap();
        assert_eq!(read_json::<String>(&path).unwrap(), Some("new".into()));
    }

    #[test]
    fn parse_errors_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        fs::write(&path, "not json").unwrap();
        let err = read_json::<Vec<u32>>(&path).unwrap_err();
        assert!(err.to_string().contains("board.json"), "error should name the file: {err}");
    }
}
