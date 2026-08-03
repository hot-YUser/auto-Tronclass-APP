//! Small, dependency-free durable file replacement.
//!
//! The temporary file is created in the destination directory, flushed, synced, then committed with
//! one OS operation. `create_new` never replaces an existing destination; `replace` does.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn create_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write(path, bytes, false)
}

pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write(path, bytes, true)
}

pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            sync_parent(parent)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn write(path: &Path, bytes: &[u8], replace: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let (temp_path, mut file) = create_temp(path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        commit(&temp_path, path, replace)?;
        sync_parent(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn create_temp(path: &Path) -> io::Result<(PathBuf, File)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("data");
    for _ in 0..8 {
        let mut random = [0_u8; 8];
        getrandom::getrandom(&mut random)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temp_path = parent.join(format!(".{name}.tmp-{suffix}"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic-write temporary file",
    ))
}

#[cfg(windows)]
fn commit(source: &Path, destination: &Path, replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let ok = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn commit(source: &Path, destination: &Path, replace: bool) -> io::Result<()> {
    if replace {
        std::fs::rename(source, destination)
    } else {
        std::fs::hard_link(source, destination)?;
        let _ = std::fs::remove_file(source);
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tron-atomic-{name}-{}", crate::config::new_id()))
    }

    #[test]
    fn create_new_refuses_existing_and_replace_is_complete() {
        let path = path("replace");
        create_new(&path, b"first").unwrap();
        let error = create_new(&path, b"must-not-win").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        replace(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let prefix = format!(".{}.tmp-", path.file_name().unwrap().to_string_lossy());
        assert!(!path.parent().unwrap().read_dir().unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().to_str().map(str::to_string))
                .is_some_and(|name| name.starts_with(&prefix))
        }));
        let _ = std::fs::remove_file(path);
    }
}
