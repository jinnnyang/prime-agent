//! The file's stable volume/file-id identity (the Windows `dev:ino`): a
//! rename keeps it, an append keeps it, a replaced file gets a fresh one —
//! exactly the inode semantics the incident-notice rotation detection (and
//! any same-file judgement) keys on. Windows references it through the
//! `GetFileInformationByHandleEx(FILE_ID_INFO)` kernel32 call as a
//! hand-declared extern wall (repo policy: pinned constants and externs,
//! no windows-sys dependency). Rust's stable metadata carries no file id on
//! Windows (`windows_by_handle` needs nightly), and NTFS "file tunneling"
//! reassigns a recreated path's creation time within its window, so the
//! cheaper metadata copies cannot answer a rotation.

use std::fs::File;

/// The volume and the file id. On the non-windows targets the index rides
/// the metadata's inode; Windows reads both off the open handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub volume: u64,
    pub index: u128,
}

#[cfg(windows)]
#[must_use]
#[allow(
    clippy::missing_panics_doc,
    clippy::as_conversions,
    clippy::ptr_as_ptr,
    clippy::cast_possible_truncation,
    reason = "the FILE_ID_INFO reflection is an FFI hand-off at the kernel32 boundary: the size is a compile-time constant and the raw-pointer hand-off is the shape the pa-types process wall already carries"
)]
pub fn file_id_from_file(file: &File) -> Option<FileId> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }

    /// `FileIdInfo` (winbase.h) in `GetFileInformationByHandleEx`'s class enum.
    const FILE_ID_INFO_CLASS: i32 = 0x12;

    extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
    }

    let mut info = FileIdInfo {
        volume_serial_number: 0,
        file_id: [0; 16],
    };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO_CLASS,
            std::ptr::from_mut::<FileIdInfo>(&mut info) as *mut c_void,
            u32::try_from(std::mem::size_of::<FileIdInfo>()).expect("FILE_ID_INFO always fits u32"),
        )
    };
    (ok != 0).then(|| FileId {
        volume: info.volume_serial_number.to_be(),
        index: info
            .file_id
            .iter()
            .fold(0u128, |hash, byte| (hash << 8) | u128::from(*byte)),
    })
}

#[cfg(unix)]
#[must_use]
pub fn file_id_from_file(file: &File) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().ok()?;
    Some(FileId {
        volume: metadata.dev(),
        index: metadata.ino().into(),
    })
}

#[cfg(not(any(unix, windows)))]
pub fn file_id_from_file(_file: &File) -> Option<FileId> {
    None
}

#[cfg(test)]
mod tests {
    use super::file_id_from_file;

    #[test]
    fn identity_stable_across_appends_and_distinguishes_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        std::fs::write(&a, b"first").expect("write a");
        std::fs::write(&b, b"second").expect("write b");
        let file = std::fs::File::open(&a).expect("open a");
        let id1 = file_id_from_file(&file).expect("identity");
        drop(file);
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&a)
            .expect("append a");
        let id2 = file_id_from_file(&file).expect("identity");
        assert_eq!(id1, id2, "appends keep the identity");
        let other = std::fs::File::open(&b).expect("open b");
        assert_ne!(
            id1,
            file_id_from_file(&other).expect("identity"),
            "distinct files"
        );
    }

    /// The rotation contract the incident-notice detection keys on: a rename
    /// keeps the identity, a recreated path is a fresh generation.
    #[test]
    fn a_rename_keeps_the_identity_and_a_replacement_releases_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("rotated.log");
        std::fs::write(&path, b"generation 1").expect("write");
        let file = std::fs::File::open(&path).expect("open");
        let id1 = file_id_from_file(&file).expect("identity");
        drop(file);

        let renamed = dir.path().join("rotated.1.log");
        std::fs::rename(&path, &renamed).expect("rename");
        let file = std::fs::File::open(&renamed).expect("open renamed");
        assert_eq!(
            id1,
            file_id_from_file(&file).expect("identity"),
            "a rename keeps the identity"
        );

        std::fs::write(&path, b"generation 2").expect("recreate");
        let file = std::fs::File::open(&path).expect("open recreated");
        assert_ne!(
            id1,
            file_id_from_file(&file).expect("identity"),
            "a replaced file gets a fresh id"
        );
    }
}
