//! Windows account identity and owner-only security descriptors.

use std::ffi::{OsStr, c_void};
use std::fmt::Write as _;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
    SECURITY_ATTRIBUTES, SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

const NONCE_BYTES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserSid(String);

impl std::fmt::Display for UserSid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Memory that the system allocated with `LocalAlloc` and that `LocalFree` releases.
struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a Win32 call that documents `LocalFree`
        // as the release function, and this type is its only owner.
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// Encodes `text` as a null-terminated UTF-16 buffer for a Win32 `PCWSTR` argument.
fn to_wide_null(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Counts the UTF-16 units before the first null in `text`.
///
/// # Safety
///
/// `text` must point to a readable, null-terminated UTF-16 string.
unsafe fn wide_len(text: *const u16) -> usize {
    let mut len = 0;
    // SAFETY: the caller guarantees a null terminator, so every read up to and
    // including the terminator stays inside the allocation.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    len
}

fn process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` is a live process handle and `token` is a valid out pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above succeeded, so `token` is a new handle that this
    // function owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn token_sid(token: &OwnedHandle) -> io::Result<UserSid> {
    let mut bytes = 0;
    // SAFETY: a null buffer with length 0 asks only for the required size.
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
    // A `usize` buffer keeps the `TOKEN_USER` cast below aligned.
    let mut buffer = vec![0_usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: `buffer` holds at least `bytes` writable bytes.
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
    // SAFETY: the call above filled `buffer` with an aligned `TOKEN_USER`.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = std::ptr::null_mut();
    // SAFETY: `user.User.Sid` points into `buffer`, which outlives this call.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(text.cast());
    // SAFETY: on success `text` is a null-terminated string that
    // `_allocation` keeps alive until this function returns.
    let units = unsafe { std::slice::from_raw_parts(text, wide_len(text)) };
    let sid = String::from_utf16(units)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(UserSid(sid))
}

/// Returns the SID of the account that runs this process.
///
/// The value is constant for the process, so the first lookup is cached.
pub(crate) fn current_user_sid() -> io::Result<&'static UserSid> {
    static CURRENT: OnceLock<UserSid> = OnceLock::new();
    if let Some(sid) = CURRENT.get() {
        return Ok(sid);
    }
    // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no release.
    let sid = token_sid(&process_token(unsafe { GetCurrentProcess() })?)?;
    Ok(CURRENT.get_or_init(|| sid))
}

pub(crate) fn process_user_sid(pid: u32) -> io::Result<UserSid> {
    // SAFETY: `OpenProcess` takes plain values and returns null on failure.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `process` is a new non-null handle that this function owns.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    token_sid(&process_token(process.as_raw_handle())?)
}

pub(crate) fn account_home() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no release.
    let token = process_token(unsafe { GetCurrentProcess() })?;
    let mut len = 0;
    // SAFETY: a null buffer asks only for the required length.
    unsafe {
        GetUserProfileDirectoryW(token.as_raw_handle(), std::ptr::null_mut(), &mut len);
    }
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u16; len as usize];
    // SAFETY: `buffer` holds `len` writable UTF-16 units.
    if unsafe { GetUserProfileDirectoryW(token.as_raw_handle(), buffer.as_mut_ptr(), &mut len) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len.saturating_sub(1) as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

/// Returns a hex string of fresh random bytes from the system RNG.
pub(crate) fn random_nonce() -> io::Result<String> {
    let mut bytes = [0_u8; NONCE_BYTES];
    // SAFETY: `bytes` is writable for `NONCE_BYTES` bytes, and a null algorithm
    // handle with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` selects the system RNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            NONCE_BYTES as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "system RNG failed with status {status:#x}"
        )));
    }
    let mut text = String::with_capacity(NONCE_BYTES * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(text)
}

/// A security descriptor that grants full access to the current account only.
pub(crate) struct OwnerSecurity(LocalAllocation);

impl OwnerSecurity {
    pub(crate) fn new() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let text = to_wide_null(OsStr::new(&format!("D:P(A;OICI;GA;;;{sid})")));
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `text` is null-terminated and `descriptor` is a valid out
        // pointer; on success the descriptor is `LocalAlloc` memory that
        // `LocalAllocation` releases.
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

    /// Borrows the descriptor for a `SECURITY_ATTRIBUTES` field.
    ///
    /// The pointer stays valid while `self` lives.
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.0.0
    }
}

/// Replaces the DACL of `path` with a protected, owner-only DACL.
///
/// This removes inherited and administrative entries. Call it only for a
/// directory that the daemon created or owns by name.
pub(crate) fn restrict_path(path: &Path) -> io::Result<()> {
    let descriptor = OwnerSecurity::new()?;
    let path = to_wide_null(path.as_os_str());
    // SAFETY: `path` is null-terminated and `descriptor` outlives the call.
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

/// Creates a new write-only file that only the current account can open.
///
/// The owner-only DACL applies at creation, so no other account can open the
/// file before its content is written. Fails with `AlreadyExists` when `path`
/// exists.
pub(crate) fn create_owner_only_file(path: &Path) -> io::Result<std::fs::File> {
    let descriptor = OwnerSecurity::new()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let path = to_wide_null(path.as_os_str());
    // SAFETY: `path` is null-terminated and `attributes` and `descriptor`
    // outlive the call.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a new valid file handle that this function owns.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}
