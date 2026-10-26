//! Private backup-file creation. Restrict access before SQLite writes any secrets.
use std::{fs::File, io, path::Path};

#[cfg(unix)]
pub(super) fn reserve(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
pub(super) use windows::reserve;

#[cfg(not(any(unix, windows)))]
pub(super) fn reserve(_: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private database backups are unsupported on this platform",
    ))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        mem::size_of,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr::null_mut,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
                ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
                SDDL_REVISION_1, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
        },
        System::{
            SystemServices::FILE_PERSISTENT_ACLS,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    // Both SID strings and security descriptors returned by these APIs use LocalAlloc.
    struct LocalAllocation(*mut c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: this allocation is uniquely owned and was returned by a Win32 API.
            unsafe { LocalFree(self.0) };
        }
    }

    fn wide_string(ptr: *const u16) -> String {
        // SAFETY: callers pass live, NUL-terminated strings allocated by Win32.
        unsafe {
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
        }
    }

    fn private_descriptor() -> io::Result<LocalAllocation> {
        // SAFETY: all output buffers are valid and remain alive through their API calls;
        // TOKEN_USER storage is pointer-aligned and sized by GetTokenInformation.
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle::from_raw_handle(token);
            let mut size = 0;
            let result =
                GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
            let error = io::Error::last_os_error();
            if result == 0 && error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                return Err(error);
            }
            if (size as usize) < size_of::<TOKEN_USER>() {
                return Err(io::Error::other("invalid process token user"));
            }
            let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            if GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut sid = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid = LocalAllocation(sid.cast());
            let sid_text = wide_string(sid.0.cast());
            // P protects the DACL from inherited directory ACEs. No Administrators,
            // Everyone or Users grant: only this token's user and Local System.
            let sddl: Vec<u16> = format!("O:{sid_text}D:P(A;;FA;;;{sid_text})(A;;FA;;;SY)")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(LocalAllocation(descriptor))
        }
    }

    fn dacl_text(descriptor: *mut c_void) -> io::Result<String> {
        // SAFETY: descriptor is a live Win32 security descriptor; the API allocates text.
        unsafe {
            let mut text = null_mut();
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let text = LocalAllocation(text.cast());
            Ok(wide_string(text.0.cast()))
        }
    }

    fn verify_acl(file: &File, expected: &LocalAllocation) -> io::Result<()> {
        // SAFETY: the handle is owned by file; all optional outputs are null except the
        // descriptor, which is owned and freed locally after inspection.
        unsafe {
            let mut flags = 0;
            if GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                &mut flags,
                null_mut(),
                0,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if flags & FILE_PERSISTENT_ACLS == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "backup volume must support persistent ACLs",
                ));
            }
            let mut actual = null_mut();
            let error = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut actual,
            );
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            let actual = LocalAllocation(actual);
            if dacl_text(actual.0)? != dacl_text(expected.0)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "backup ACL was not preserved",
                ));
            }
        }
        Ok(())
    }

    pub(in super::super) fn reserve(path: &Path) -> io::Result<File> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if path[..path.len() - 1].contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "backup path contains NUL",
            ));
        }
        let descriptor = private_descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: path and descriptor remain live for CreateFileW. CREATE_NEW refuses
        // existing files, including links. The returned handle is uniquely owned by File.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_handle(handle) };
        // Refuse filesystems that silently ignore ACLs before writing any database data.
        verify_acl(&file, &descriptor)?;
        Ok(file)
    }
}
