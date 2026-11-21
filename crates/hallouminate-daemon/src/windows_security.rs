//! Windows account identity and owner-only security descriptors.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
    SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserSid(String);

impl std::fmt::Display for UserSid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn token_sid(token: &OwnedHandle) -> io::Result<UserSid> {
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        );
    }
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let allocation = LocalAllocation(text.cast());
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    drop(allocation);
    Ok(UserSid(sid))
}

pub(crate) fn current_user_sid() -> io::Result<UserSid> {
    token_sid(&process_token(unsafe { GetCurrentProcess() })?)
}

pub(crate) fn process_user_sid(pid: u32) -> io::Result<UserSid> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    token_sid(&process_token(process.as_raw_handle())?)
}

pub(crate) fn account_home() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    let token = process_token(unsafe { GetCurrentProcess() })?;
    let mut len = 0;
    unsafe {
        GetUserProfileDirectoryW(token.as_raw_handle(), std::ptr::null_mut(), &mut len);
    }
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u16; len as usize];
    if unsafe { GetUserProfileDirectoryW(token.as_raw_handle(), buffer.as_mut_ptr(), &mut len) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len.saturating_sub(1) as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

pub(crate) struct OwnerSecurity(LocalAllocation);

impl OwnerSecurity {
    pub(crate) fn new() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let mut text = Vec::new();
        for unit in format!("D:P(A;OICI;GA;;;{sid})").encode_utf16() {
            text.push(unit);
        }
        text.push(0);
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalAllocation(descriptor)))
    }

    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.0.0
    }
}

pub(crate) fn restrict_path(path: &Path) -> io::Result<()> {
    let descriptor = OwnerSecurity::new()?;
    let mut wide = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        wide.push(unit);
    }
    let mut path = wide;
    path.push(0);
    if unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor.as_ptr(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
