use std::fs::Metadata;
use std::path::Path;

/// Returns true when a file has multiple hard links.
///
/// Multiple links can allow path-based workspace guards to be bypassed by
/// linking a workspace path to external sensitive content. `path` is the
/// file `metadata` was read from: Windows only reports the link count
/// through an open handle.
pub fn has_multiple_hard_links(metadata: &Metadata, path: &Path) -> bool {
    link_count(metadata, path) > 1
}

#[cfg(unix)]
fn link_count(metadata: &Metadata, _path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

/// elfClaw 2026-10-20: this used to return 1 unconditionally ("hard links are
/// uncommon on Windows"), so the guard never fired on the platform elfClaw
/// runs on — NTFS hard links need no privilege. The count now comes from
/// `GetFileInformationByHandle`. A file that cannot be opened counts as one
/// link: the caller's own read or write of it fails anyway.
#[cfg(windows)]
#[allow(unsafe_code)]
fn link_count(metadata: &Metadata, path: &Path) -> u64 {
    use std::os::windows::io::AsRawHandle;

    // BY_HANDLE_FILE_INFORMATION
    #[repr(C)]
    #[derive(Default)]
    struct FileInfo {
        attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        size_high: u32,
        size_low: u32,
        number_of_links: u32,
        index_high: u32,
        index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(file: *mut std::ffi::c_void, info: *mut FileInfo) -> i32;
    }

    if !metadata.is_file() {
        return 1;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return 1;
    };
    let mut info = FileInfo::default();
    // SAFETY: the handle is open for the duration of the call and `info` is
    // a properly sized, writable BY_HANDLE_FILE_INFORMATION.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) };
    if ok == 0 {
        return 1;
    }
    u64::from(info.number_of_links)
}

#[cfg(not(any(unix, windows)))]
fn link_count(_metadata: &Metadata, _path: &Path) -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_link_file_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("single.txt");
        std::fs::write(&file, "hello").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert!(!has_multiple_hard_links(&meta, &file));
    }

    #[test]
    fn hard_link_file_is_flagged_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.txt");
        let linked = dir.path().join("linked.txt");
        std::fs::write(&original, "hello").unwrap();

        if std::fs::hard_link(&original, &linked).is_err() {
            // Some filesystems may disable hard links; treat as unsupported.
            return;
        }

        let meta = std::fs::metadata(&original).unwrap();
        assert!(has_multiple_hard_links(&meta, &original));
        let meta = std::fs::metadata(&linked).unwrap();
        assert!(has_multiple_hard_links(&meta, &linked));
    }
}
