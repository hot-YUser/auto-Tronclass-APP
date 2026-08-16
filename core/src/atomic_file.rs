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

/// 標註是哪一步失敗。沒有這個,平台差異造成的 EACCES 只能用猜的 —— 建暫存檔?hard_link?
/// 對目錄 fsync?實際在 Android 上就踩過:訊息只有「Permission denied」,完全無從下手。
/// 保留原本的 `ErrorKind`(呼叫端仍可用 kind 判斷),只在訊息前面加上步驟名。
/// io::Error 的 Display 在 Rust 標準庫中**不含路徑或檔案內容**,故可安全跨過 FFI 縫。
fn at(step: &'static str) -> impl FnOnce(io::Error) -> io::Error {
    move |error| io::Error::new(error.kind(), format!("{step}：{error}"))
}

fn write(path: &Path, bytes: &[u8], replace: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(at("建立目錄"))?;
    let (temp_path, mut file) = create_temp(path).map_err(at("建立暫存檔"))?;
    let result = (|| {
        file.write_all(bytes).map_err(at("寫入暫存檔"))?;
        file.flush().map_err(at("flush 暫存檔"))?;
        file.sync_all().map_err(at("fsync 暫存檔"))?;
        drop(file);
        commit(&temp_path, path, replace).map_err(at(if replace {
            "rename 就位"
        } else {
            "hard_link 就位"
        }))?;
        sync_parent(parent).map_err(at("fsync 父目錄"))?;
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
        getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
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
        return std::fs::rename(source, destination);
    }
    // 首選 hard_link:一個 syscall 同時保住兩個性質 —— 既有檔存在就失敗(不覆蓋),
    // 且讀者永遠看不到半截內容。
    match std::fs::hard_link(source, destination) {
        Ok(()) => {
            let _ = std::fs::remove_file(source);
            Ok(())
        }
        // Android 的 app 私有目錄禁止 link(2)(SELinux ＋ protected_hardlinks),回 EACCES/EPERM。
        // 實測 OnePlus 13 / Android 16:providers.json 建不出來 → 核心 Init 失敗 → 整個 App
        // 開不起來;vault.bin 與裝置金鑰走同一條路,同樣建不出來。故此處必須有後備。
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) =>
        {
            commit_via_placeholder(source, destination)
        }
        Err(error) => Err(error),
    }
}

/// hard_link 不可用時的後備，刻意保住與 hard_link 相同的兩個性質：
/// 1. **不覆蓋**：`O_CREAT|O_EXCL` 原子地佔住檔名，既有檔存在就回 `AlreadyExists`；
/// 2. **不露半截**：佔位成功後，把已寫滿並 fsync 過的暫存檔 `rename` 蓋掉自己的佔位檔，
///    rename 本身是原子的。
///
/// 與 hard_link 的唯一差別：若在「佔位」與「rename」之間程序被殺，會留下一個空檔。
/// 讀者端本來就全數驗證內容（vault 走 AEAD、裝置金鑰驗長度、providers.json 解析失敗會隔離），
/// 空檔會被當成毀損處理，不會被誤認為有效資料。
#[cfg(not(windows))]
fn commit_via_placeholder(source: &Path, destination: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(destination)?;
    std::fs::rename(source, destination)
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

    /// hard_link 後備路徑必須保住與 hard_link 相同的兩個性質。Android 的 app 私有目錄禁止
    /// link(2)（EACCES），所有 `create_new` 的使用者——providers.json、vault.bin、裝置金鑰
    /// ——都會走到這裡；它一旦破功，整個 App 在 Android 上就開不起來。
    #[cfg(not(windows))]
    #[test]
    fn placeholder_commit_refuses_existing_and_lands_complete_content() {
        // 1) 目的檔已存在 → 必須 AlreadyExists，且原內容不被動到。
        let occupied = path("placeholder-occupied");
        create_new(&occupied, b"incumbent").unwrap();
        let temp = path("placeholder-temp-a");
        std::fs::write(&temp, b"challenger").unwrap();
        let error = commit_via_placeholder(&temp, &occupied).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"incumbent");
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::remove_file(&occupied);

        // 2) 目的檔不存在 → 內容完整就位，暫存檔不殘留。
        let fresh = path("placeholder-fresh");
        let temp = path("placeholder-temp-b");
        std::fs::write(&temp, b"complete-payload").unwrap();
        commit_via_placeholder(&temp, &fresh).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"complete-payload");
        assert!(!temp.exists());
        let _ = std::fs::remove_file(fresh);
    }
}
