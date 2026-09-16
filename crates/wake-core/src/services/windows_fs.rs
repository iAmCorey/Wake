//! Filesystem evidence used by cleanup. Query one open handle rather than using
//! logical file length or a placeholder identity on Windows.
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs::{Metadata, OpenOptions},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Component, Path, Prefix},
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        FileCompressionInfo, FileIdInfo, FileStandardInfo, GetDriveTypeW,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetVolumePathNameW,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_SPARSE_FILE, FILE_COMPRESSION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    },
    System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE},
};

pub(crate) fn ensure_recyclable(path: &Path) -> Result<()> {
    // Reject UNC before any I/O, including extended UNC. trash's Shell backend
    // strips the extended prefix, which would also corrupt an extended UNC path.
    if matches!(path.components().next(), Some(Component::Prefix(p))
        if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
    {
        bail!("Network locations cannot be moved to the Recycle Bin");
    }
    ensure!(
        path.is_absolute(),
        "Source path contains links or is not absolute"
    );
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut volume = vec![0; 32_768];
    // SAFETY: both strings are NUL-terminated, the output buffer is writable,
    // and its size is supplied in UTF-16 code units.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot verify the source volume");
    }
    // Includes drive-letter mappings and mounted volumes, not only UNC syntax.
    match unsafe { GetDriveTypeW(volume.as_ptr()) } {
        DRIVE_FIXED => Ok(()),
        DRIVE_REMOTE => bail!("Network locations cannot be moved to the Recycle Bin"),
        _ => bail!("This location does not support safe recycling"),
    }
}

pub(crate) struct FileSnapshot {
    pub metadata: Metadata,
    pub bytes: u64,
    pub identity: (u64, u64),
    pub identity_high: u64,
}

pub(crate) fn snapshot(path: &Path) -> Result<FileSnapshot> {
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let handle = file.as_raw_handle();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the owned File keeps the handle live, and info has the API's layout.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot verify file identity");
    }
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "Symbolic links are not supported"
    );
    let metadata = file.metadata()?;
    ensure!(
        !metadata.is_file() || info.nNumberOfLinks == 1,
        "Shared hard links are not supported"
    );
    // ReFS uses 128-bit IDs; the legacy 64-bit file index is not sufficient.
    let mut id = FILE_ID_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot verify file identity");
    }
    ensure!(
        id.FileId.Identifier != [0; 16],
        "Cannot verify file identity"
    );
    let low = u64::from_le_bytes(id.FileId.Identifier[..8].try_into()?);
    let high = u64::from_le_bytes(id.FileId.Identifier[8..].try_into()?);
    let bytes = if metadata.is_dir() {
        0
    } else if info.dwFileAttributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0
    {
        let mut compression = FILE_COMPRESSION_INFO::default();
        // Compressed/sparse streams need their actual allocation, not EOF or
        // the standard-info allocation of their uncompressed logical ranges.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileCompressionInfo,
                (&mut compression as *mut FILE_COMPRESSION_INFO).cast(),
                std::mem::size_of::<FILE_COMPRESSION_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("Cannot measure disk usage");
        }
        u64::try_from(compression.CompressedFileSize)?
    } else {
        let mut standard = FILE_STANDARD_INFO::default();
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("Cannot measure disk usage");
        }
        u64::try_from(standard.AllocationSize)?
    };
    Ok(FileSnapshot {
        metadata,
        bytes,
        identity: (id.VolumeSerialNumber, low),
        identity_high: high,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_locations_are_rejected_without_accessing_the_share() {
        for path in [
            r"\\server\share\session.jsonl",
            r"\\?\UNC\server\share\session.jsonl",
        ] {
            assert_eq!(
                ensure_recyclable(Path::new(path)).unwrap_err().to_string(),
                "Network locations cannot be moved to the Recycle Bin"
            );
        }
    }

    #[test]
    fn local_volume_and_allocated_size_are_available() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, vec![b'a'; 8193]).unwrap();
        ensure_recyclable(&path).unwrap();
        let file = snapshot(&path).unwrap();
        assert!(file.bytes >= file.metadata.len());
        assert_ne!((file.identity.1, file.identity_high), (0, 0));
        assert_eq!(snapshot(&path).unwrap().identity, file.identity);
    }

    #[test]
    fn sparse_and_compressed_files_use_disk_allocation_not_logical_length() {
        use windows_sys::Win32::{
            Storage::FileSystem::COMPRESSION_FORMAT_DEFAULT,
            System::{
                Ioctl::{FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE},
                IO::DeviceIoControl,
            },
        };
        let temp = tempfile::tempdir().unwrap();
        for sparse in [true, false] {
            let path = temp
                .path()
                .join(if sparse { "sparse" } else { "compressed" });
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .read(true)
                .open(&path)
                .unwrap();
            let compression = COMPRESSION_FORMAT_DEFAULT;
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    if sparse {
                        FSCTL_SET_SPARSE
                    } else {
                        FSCTL_SET_COMPRESSION
                    },
                    if sparse {
                        std::ptr::null()
                    } else {
                        (&compression as *const u16).cast()
                    },
                    if sparse {
                        0
                    } else {
                        std::mem::size_of::<u16>() as u32
                    },
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(
                ok,
                0,
                "NTFS fixture setup failed: {}",
                std::io::Error::last_os_error()
            );
            if sparse {
                file.set_len(16 * 1024 * 1024).unwrap();
            } else {
                use std::io::Write;
                (&file).write_all(&vec![b'a'; 1024 * 1024]).unwrap();
            }
            file.sync_all().unwrap();
            drop(file);
            let FileSnapshot {
                metadata, bytes, ..
            } = snapshot(&path).unwrap();
            assert!(
                bytes < metadata.len() / 2,
                "logical={}, allocated={bytes}",
                metadata.len()
            );
        }
    }
}
