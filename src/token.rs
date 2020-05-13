use crate::sid::{get_length_sid, is_well_known, AsSid, WellKnownSid};
use crate::win32_error_with_context;
use std::io::{Error as IoError, Result as IoResult};
use winapi::shared::minwindef::{BOOL, DWORD, FALSE};
use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess, OpenProcessToken};
use winapi::um::securitybaseapi::{
    CheckTokenMembership, DuplicateTokenEx, GetTokenInformation, ImpersonateLoggedOnUser,
    SetTokenInformation,
};
use winapi::um::winnt::{
    SecurityImpersonation, TokenElevationType, TokenElevationTypeFull, TokenImpersonation,
    TokenIntegrityLevel, TokenPrimary, WinBuiltinAdministratorsSid, WinHighLabelSid,
    WinMediumLabelSid, HANDLE, PROCESS_QUERY_INFORMATION, SE_GROUP_INTEGRITY, SID,
    SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_ELEVATION_TYPE, TOKEN_IMPERSONATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    TOKEN_TYPE,
};
use winapi::um::winsafer::{
    SaferCloseLevel, SaferComputeTokenFromLevel, SaferCreateLevel, SAFER_LEVELID_NORMALUSER,
    SAFER_LEVEL_HANDLE, SAFER_LEVEL_OPEN, SAFER_SCOPEID_USER,
};
use winapi::um::winuser::{GetShellWindow, GetWindowThreadProcessId};

/// Indicates the effective level of privileges held by the token
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeLevel {
    /// The token isn't privileged OR may be privileged in
    /// an unusual way that we don't know how to guarantee
    /// that we can/should reduce privilege or do so successfully.
    NotPrivileged,
    /// The token is an elevated token produced via runas/UAC
    Elevated,
    /// The token isn't an elevated token but it does have
    /// high integrity privileges, such as those produced
    /// by sshing in to a Windows 10 system.
    HighIntegrityAdmin,
}

/// A helper that wraps a TOKEN_MANDATORY_LABEL struct.
/// That struct holds a SID and some attribute flags.
/// Its use in this module is to query the integrity level
/// of the token, so we have a very targeted set of accessors
/// for that purpose.
/// The integrity level is a single SID that represents the
/// degree of trust that the token has.
/// A normal user is typically running with Medium integrity,
/// whereas an elevated session is typically running with High
/// integrity.
struct TokenIntegrityLevel {
    data: Vec<u8>,
}

impl TokenIntegrityLevel {
    fn as_label(&self) -> &TOKEN_MANDATORY_LABEL {
        // This is safe because we cannot construct an invalid instance
        unsafe { &*(self.data.as_ptr() as *const TOKEN_MANDATORY_LABEL) }
    }

    fn sid(&self) -> *const SID {
        // For whatever reason, the PSID type in the SDK is defined
        // as void* and that is the type of Label.Sid, rather than
        // SID*, so we get to cast it here.
        self.as_label().Label.Sid as *const SID
    }

    /// Return true if this is a high integrity level label
    pub fn is_high(&self) -> bool {
        is_well_known(self.sid(), WinHighLabelSid)
    }
}

/// `Token` represents a set of credentials and privileges.  A process
/// typically inherits the token of its parent process for its primary
/// token, and Windows allows for threads to create/obtain impersonation
/// tokens so that a thread can run with a different identity for a
/// while.
///
/// For the purposes of this crate, we are concerned with reducing
/// the scope of the privileges in a given Token.
pub struct Token {
    pub(crate) token: HANDLE,
}

impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.token);
        }
    }
}

impl Token {
    /// Obtain a handle to the primary token for this process
    pub fn with_current_process() -> IoResult<Self> {
        let mut token: HANDLE = INVALID_HANDLE_VALUE;
        let res = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
                &mut token,
            )
        };
        if res != 1 {
            Err(win32_error_with_context(
                "OpenProcessToken(GetCurrentProcess))",
                IoError::last_os_error(),
            ))
        } else {
            Ok(Self { token })
        }
    }

    /// Obtain the token from the shell process as a primary token.
    /// This can fail if there is no shell accessible to the process,
    /// for example if the process was spawned by an ssh session.
    /// Why might we want this token?  We can't directly
    /// de-elevate a token so we need to obtain a non-elevated
    /// token from a well known source.
    pub fn with_shell_process() -> IoResult<Self> {
        let shell_window = unsafe { GetShellWindow() };
        if shell_window.is_null() {
            return Err(IoError::new(
                std::io::ErrorKind::NotFound,
                "there is no shell window",
            ));
        }

        let mut shell_pid: DWORD = 0;
        let _thread_id = unsafe { GetWindowThreadProcessId(shell_window, &mut shell_pid) };

        let proc = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, FALSE, shell_pid) };
        if proc == INVALID_HANDLE_VALUE {
            return Err(win32_error_with_context(
                "OpenProcess(shell_pid)",
                IoError::last_os_error(),
            ));
        }

        struct Process(HANDLE);
        impl Drop for Process {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let proc = Process(proc);

        let mut token: HANDLE = INVALID_HANDLE_VALUE;
        let res = unsafe { OpenProcessToken(proc.0, TOKEN_DUPLICATE, &mut token) };
        if res != 1 {
            Err(win32_error_with_context(
                "OpenProcessToken(shell process)",
                IoError::last_os_error(),
            ))
        } else {
            let token = Self { token };

            // And now that we have it, make a primary token from it!
            token.duplicate_as_primary_token()
        }
    }

    /// Build a medium integrity level normal user access token
    /// from the current token.
    /// This is most suitable in the case where you have a
    /// HighIntegrityAdmin privilege level and want to proceed
    /// with a normal privilege token.
    pub fn as_medium_integrity_safer_token(&self) -> IoResult<Self> {
        let mut level: SAFER_LEVEL_HANDLE = std::ptr::null_mut();
        let res = unsafe {
            SaferCreateLevel(
                SAFER_SCOPEID_USER,
                SAFER_LEVELID_NORMALUSER,
                SAFER_LEVEL_OPEN,
                &mut level,
                std::ptr::null_mut(),
            )
        };
        if res != 1 {
            return Err(win32_error_with_context(
                "SaferCreateLevel",
                IoError::last_os_error(),
            ));
        }

        struct SaferHandle(SAFER_LEVEL_HANDLE);
        impl Drop for SaferHandle {
            fn drop(&mut self) {
                unsafe { SaferCloseLevel(self.0) };
            }
        }
        let level = SaferHandle(level);

        let mut token = INVALID_HANDLE_VALUE;
        let res = unsafe {
            SaferComputeTokenFromLevel(level.0, self.token, &mut token, 0, std::ptr::null_mut())
        };
        if res != 1 {
            return Err(win32_error_with_context(
                "SaferComputeTokenFromLevel",
                IoError::last_os_error(),
            ));
        }

        let token = Self { token };
        token.set_medium_integrity()?;
        Ok(token)
    }

    fn set_medium_integrity(&self) -> IoResult<()> {
        let medium = WellKnownSid::with_well_known(WinMediumLabelSid)?;
        let mut tml = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Attributes: SE_GROUP_INTEGRITY,
                Sid: medium.as_sid() as *mut _,
            },
        };

        let res = unsafe {
            SetTokenInformation(
                self.token,
                TokenIntegrityLevel,
                &mut tml as *mut TOKEN_MANDATORY_LABEL as *mut _,
                std::mem::size_of_val(&tml) as u32 + get_length_sid(&medium),
            )
        };
        if res != 1 {
            Err(win32_error_with_context(
                "SetTokenInformation(TokenIntegrityLevel Medium)",
                IoError::last_os_error(),
            ))
        } else {
            Ok(())
        }
    }

    /// Attempt to duplicate this token as one that is suitable
    /// for use in impersonation related APIs, which includes the
    /// check_membership method.
    fn duplicate_as_impersonation_token(&self) -> IoResult<Self> {
        self.duplicate(TokenImpersonation)
    }

    fn duplicate_as_primary_token(&self) -> IoResult<Self> {
        self.duplicate(TokenPrimary)
    }

    fn duplicate(&self, token_type: TOKEN_TYPE) -> IoResult<Self> {
        let mut dup: HANDLE = INVALID_HANDLE_VALUE;
        let res = unsafe {
            DuplicateTokenEx(
                self.token,
                TOKEN_ADJUST_SESSIONID
                    | TOKEN_ADJUST_DEFAULT
                    | TOKEN_ASSIGN_PRIMARY
                    | TOKEN_IMPERSONATE
                    | TOKEN_DUPLICATE
                    | TOKEN_QUERY,
                std::ptr::null_mut(),
                SecurityImpersonation,
                token_type,
                &mut dup,
            )
        };
        if res != 1 {
            Err(win32_error_with_context(
                "DuplicateTokenEx",
                IoError::last_os_error(),
            ))
        } else {
            Ok(Self { token: dup })
        }
    }

    /// Returns true if `sid` is an enabled group on this token.
    /// The token must be an impersonation token, so you may need
    /// to use duplicate_as_impersonation_token() to obtain one.
    fn check_membership<S: AsSid>(&self, sid: S) -> IoResult<bool> {
        let mut is_member: BOOL = 0;
        let res =
            unsafe { CheckTokenMembership(self.token, sid.as_sid() as *mut _, &mut is_member) };
        if res != 1 {
            Err(win32_error_with_context(
                "CheckTokenMembership",
                IoError::last_os_error(),
            ))
        } else {
            Ok(is_member == 1)
        }
    }

    /// A convenience wrapper around check_membership that tests for
    /// being a member of the builtin administrators group
    fn check_administrators_membership(&self) -> IoResult<bool> {
        let admins = WellKnownSid::with_well_known(WinBuiltinAdministratorsSid)?;
        self.check_membership(&admins)
    }

    /// Retrieve the integrity level label of the process.
    fn integrity_level(&self) -> IoResult<TokenIntegrityLevel> {
        let mut size: DWORD = 0;
        let err;

        unsafe {
            GetTokenInformation(
                self.token,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut size,
            );
            err = GetLastError();
        };

        // The call should have failed and told us we need more space
        if err != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32_error_with_context(
                "GetTokenInformation TokenIntegrityLevel unexpected failure",
                IoError::last_os_error(),
            ));
        }

        // Allocate and zero out the storage
        let mut data = vec![0u8; size as usize];

        unsafe {
            if GetTokenInformation(
                self.token,
                TokenIntegrityLevel,
                data.as_mut_ptr() as *mut _,
                size,
                &mut size,
            ) == 0
            {
                return Err(win32_error_with_context(
                    "GetTokenInformation TokenIntegrityLevel",
                    IoError::last_os_error(),
                ));
            }
        };

        Ok(TokenIntegrityLevel { data })
    }

    /// Return an enum value that indicates the degree of elevation
    /// applied to the current token; this can be one of:
    /// TokenElevationTypeDefault, TokenElevationTypeFull,
    /// TokenElevationTypeLimited.
    fn elevation_type(&self) -> IoResult<TOKEN_ELEVATION_TYPE> {
        let mut ele_type: TOKEN_ELEVATION_TYPE = 0;
        let mut size: DWORD = 0;
        let res = unsafe {
            GetTokenInformation(
                self.token,
                TokenElevationType,
                &mut ele_type as *mut TOKEN_ELEVATION_TYPE as *mut _,
                std::mem::size_of_val(&ele_type) as u32,
                &mut size,
            )
        };
        if res != 1 {
            Err(win32_error_with_context(
                "GetTokenInformation TOKEN_ELEVATION_TYPE",
                IoError::last_os_error(),
            ))
        } else {
            Ok(ele_type)
        }
    }

    /// Determine the effective privilege level of the token
    pub fn privilege_level(&self) -> IoResult<PrivilegeLevel> {
        let ele_type = self.elevation_type()?;
        if ele_type == TokenElevationTypeFull {
            return Ok(PrivilegeLevel::Elevated);
        }

        let level = self.integrity_level()?;
        if !level.is_high() {
            return Ok(PrivilegeLevel::NotPrivileged);
        }

        let imp_token = self.duplicate_as_impersonation_token()?;
        if imp_token.check_administrators_membership()? {
            Ok(PrivilegeLevel::HighIntegrityAdmin)
        } else {
            Ok(PrivilegeLevel::NotPrivileged)
        }
    }

    /// Impersonate applies the token to the current thread only.
    /// This isn't a supported API: it is present to furnish an
    /// example that shows that it doesn't behave how you might
    /// expect!
    #[doc(hidden)]
    pub fn impersonate(&self) -> IoResult<()> {
        let res = unsafe { ImpersonateLoggedOnUser(self.token) };
        if res != 1 {
            Err(win32_error_with_context(
                "ImpersonateLoggedOnUser",
                IoError::last_os_error(),
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn get_own_token() {
        let token = Token::with_current_process().unwrap();
        let level = token.privilege_level().unwrap();
        // We can't make any assertions about what the level is,
        // but we can at least assume that we should be able
        // to successfully reach this point
        eprintln!("priv level is {:?}", level);

        // Verify that we can build a medium token from this,
        // and that the medium token doesn't show as privileged
        let medium = token.as_medium_integrity_safer_token().unwrap();
        let level = medium.privilege_level().unwrap();
        assert_eq!(level, PrivilegeLevel::NotPrivileged);
    }

    #[test]
    fn get_shell_token() {
        // We should either successfully obtain the shell token (if we're
        // connected to a desktop with a shell), or get a NotFound error.
        // We treat any other error as a test failure.
        match Token::with_shell_process() {
            Ok(_) => eprintln!("got shell token!"),
            Err(err) => match err.kind() {
                std::io::ErrorKind::NotFound => eprintln!("There is no shell"),
                _ => panic!("failed to get shell token: {:?}", err),
            },
        }
    }
}
