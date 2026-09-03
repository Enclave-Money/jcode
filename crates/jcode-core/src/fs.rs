use std::path::Path;

/// Set file permissions to owner-only read/write (0o600).
/// On Windows, replaces the DACL with a protected full-control ACE for the
/// current process user so inherited or explicit access for other principals
/// cannot expose secret-bearing files.
pub fn set_permissions_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
    }
    #[cfg(windows)]
    {
        set_windows_acl_owner_only(path, 0)
    }
}

/// Open a socket to one named group, and to nobody else (0o660, group `group`).
///
/// For team servers: each room is a daemon running as its own Unix user, and a
/// single public door must reach every room's socket. The group is expected to
/// hold ONLY the door, so members still cannot connect to each other's daemons
/// — the isolation that matters survives, and the one process that has to
/// cross it can.
///
/// Fails rather than silently loosening, so an unknown group leaves the caller
/// to fall back to owner-only.
#[cfg(unix)]
pub fn share_socket_with_group(path: &Path, group: &str) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let name = CString::new(group).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "group name has a NUL")
    })?;
    // getgrnam returns a pointer into a static buffer: read the gid out
    // immediately rather than holding it.
    let gid = unsafe {
        let entry = libc::getgrnam(name.as_ptr());
        if entry.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no such group: {group}"),
            ));
        }
        (*entry).gr_gid
    };
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has a NUL"))?;
    // u32::MAX (-1) leaves the owner unchanged.
    if unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
}

#[cfg(not(unix))]
pub fn share_socket_with_group(_path: &Path, _group: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "group-shared sockets are a Unix concept",
    ))
}

/// Open a socket to exactly ONE other local user, via a POSIX ACL.
///
/// The team-server split needs the public door's user to reach each room's
/// socket. Doing that with a shared gid on the room daemon (systemd `Group=`)
/// was audit finding R1: a gid is inherited by every subprocess, so each
/// member's AGENT held the door group — and with it every other room's socket
/// and X cookie. An unprivileged process cannot drop a gid, but a file's
/// owner may always set ACLs on it, so this grants the door and nobody else
/// while the daemon runs with no shared group at all.
#[cfg(target_os = "linux")]
pub fn share_socket_with_user(path: &Path, user: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if user.is_empty()
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        || user.starts_with('-')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a plausible username: {user:?}"),
        ));
    }
    // Close group/other first. setfacl recomputes the mask from the named
    // entry it adds, so the door's grant survives while nothing else opens.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let status = std::process::Command::new("setfacl")
        .arg("-m")
        .arg(format!("u:{user}:rw"))
        .arg(path)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "setfacl u:{user}:rw exited with {status}"
        )))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn share_socket_with_user(_path: &Path, _user: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "per-user socket ACLs are used only on Linux team servers",
    ))
}

/// Set directory permissions to owner-only read/write/execute (0o700).
/// Windows child objects inherit the same current-user-only access rule.
pub fn set_directory_permissions_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::SUB_CONTAINERS_AND_OBJECTS_INHERIT;
        set_windows_acl_owner_only(path, SUB_CONTAINERS_AND_OBJECTS_INHERIT)
    }
}

#[cfg(windows)]
fn set_windows_acl_owner_only(
    path: &Path,
    inheritance: windows_sys::Win32::Security::ACE_FLAGS,
) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_ALL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
        TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| {
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }

        // TOKEN_USER contains pointer-aligned data followed by its SID. A usize
        // buffer gives the cast the alignment required by the Windows ABI.
        let word = std::mem::size_of::<usize>();
        let mut token_user = vec![0usize; (needed as usize).div_ceil(word)];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_user.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let user = unsafe { &*(token_user.as_ptr().cast::<TOKEN_USER>()) };

        let trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.User.Sid.cast::<u16>(),
        };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: SET_ACCESS,
            grfInheritance: inheritance,
            Trustee: trustee,
        };
        let mut acl = null_mut();
        let acl_status = unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) };
        if acl_status != 0 {
            return Err(std::io::Error::from_raw_os_error(acl_status as i32));
        }

        let mut wide_path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let security_status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null(),
            )
        };
        unsafe {
            LocalFree(acl.cast::<c_void>());
        }
        if security_status != 0 {
            return Err(std::io::Error::from_raw_os_error(security_status as i32));
        }
        Ok(())
    })();

    unsafe {
        CloseHandle(token);
    }
    result
}

#[cfg(all(test, unix))]
mod share_socket_tests {
    use super::*;

    /// An unknown group must FAIL rather than quietly leaving the socket at
    /// whatever the umask produced. The daemon relies on that error to fall
    /// back to owner-only; a silent success would publish a room's socket.
    #[test]
    fn an_unknown_group_is_an_error_not_a_silent_loosening() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("s.sock");
        std::fs::write(&path, b"").unwrap();

        let error = share_socket_with_group(&path, "no-such-group-for-tests")
            .expect_err("an unknown group must not be treated as success");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    /// Sharing with the caller's OWN primary group is always possible, so this
    /// exercises the success path without needing root or a fixture group.
    #[test]
    fn sharing_sets_group_read_write_and_nothing_wider() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("s.sock");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let gid = std::fs::metadata(&path).unwrap().gid();
        let group = unsafe {
            let entry = libc::getgrgid(gid);
            if entry.is_null() {
                return; // No name for this gid in the test environment.
            }
            std::ffi::CStr::from_ptr((*entry).gr_name)
                .to_string_lossy()
                .into_owned()
        };

        share_socket_with_group(&path, &group).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "group rw, and closed to everyone else");
        assert_eq!(mode & 0o007, 0, "world must have no access at all");
    }
}
