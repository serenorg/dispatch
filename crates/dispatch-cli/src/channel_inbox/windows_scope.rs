//! Windows provisioning for the channel-state directory only. Journal bytes,
//! SQLite persistence, and the general Dispatch secret store are unchanged.

use anyhow::{Context, Result, bail};
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, GetTokenInformation, IsValidAcl,
        IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SID,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetVolumeInformationByHandleW, READ_CONTROL,
    },
    System::{
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, FILE_PERSISTENT_ACLS},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

#[derive(Clone, Copy)]
enum Provisioning {
    CreateMissing,
    ExistingOnly,
}

pub(super) fn ensure(directory: &Path) -> Result<()> {
    provision(directory, Provisioning::CreateMissing).with_context(|| guidance(directory))
}

pub(super) fn validate(directory: &Path) -> Result<()> {
    provision(directory, Provisioning::ExistingOnly).with_context(|| guidance(directory))
}

fn guidance(directory: &Path) -> String {
    format!(
        "channel scope {} must be owned by the process account and private to that account, SYSTEM, and Administrators; provision a private scope on a direct persistent path without changing or deleting existing channel data",
        directory.display()
    )
}

fn checked_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("channel scope requires an absolute path");
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!("channel scope must not contain parent traversal");
        }
        if let Component::Normal(name) = component {
            if name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".agent-shell"))
                || name.encode_wide().any(|unit| unit == u16::from(b':'))
            {
                bail!(
                    "channel scope must not select protected session data or an alternate stream"
                );
            }
            // Win32 normalization strips trailing dots and spaces and maps DOS
            // device names, so such a component can alias another object than
            // the verbatim path used for creation.
            if name.to_str().is_none_or(is_win32_alias) {
                bail!("channel scope components must be plain Unicode names without Win32 aliases");
            }
        }
    }
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        bail!("channel scope contains NUL");
    }
    Ok(())
}

fn is_win32_alias(name: &str) -> bool {
    const DEVICES: [&str; 28] = [
        "CON",
        "PRN",
        "AUX",
        "NUL",
        "COM1",
        "COM2",
        "COM3",
        "COM4",
        "COM5",
        "COM6",
        "COM7",
        "COM8",
        "COM9",
        "LPT1",
        "LPT2",
        "LPT3",
        "LPT4",
        "LPT5",
        "LPT6",
        "LPT7",
        "LPT8",
        "LPT9",
        "COM\u{b9}",
        "COM\u{b2}",
        "COM\u{b3}",
        "LPT\u{b9}",
        "LPT\u{b2}",
        "LPT\u{b3}",
    ];
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    name.ends_with(['.', ' '])
        || DEVICES
            .iter()
            .any(|device| stem.eq_ignore_ascii_case(device))
}

fn provision(directory: &Path, provisioning: Provisioning) -> Result<()> {
    checked_path(directory)?;
    let account = Account::current()?;
    let descriptor = account.directory_descriptor()?;
    let mut ancestors: Vec<_> = directory.ancestors().collect();
    ancestors.reverse();
    // Denying delete sharing pins each checked ancestor while missing scope
    // components are created. Existing project ancestors are never changed.
    let mut handles = Vec::new();
    for ancestor in ancestors {
        let file = match open_directory(ancestor) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && matches!(provisioning, Provisioning::CreateMissing) =>
            {
                let parent = handles
                    .last()
                    .context("channel scope has no existing parent")?;
                require_acl_volume(parent)?;
                match create_directory(ancestor, &descriptor) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                let file = open_directory(ancestor)?;
                validate_private(&file, &account)?;
                file
            }
            Err(error) => return Err(error.into()),
        };
        checked_path(&fs::canonicalize(ancestor)?)?;
        handles.push(file);
    }
    let scope = handles.last().context("channel scope handle is absent")?;
    require_acl_volume(scope)?;
    validate_private(scope, &account)
}

fn open_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "channel scope path must contain plain directories, not reparse points",
        ));
    }
    Ok(file)
}

fn require_acl_volume(file: &File) -> Result<()> {
    let mut flags = 0;
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    if flags & FILE_PERSISTENT_ACLS == 0 {
        bail!("channel scope volume must support persistent ACLs");
    }
    Ok(())
}

fn create_directory(path: &Path, descriptor: &LocalAllocation) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "channel scope has no parent")
    })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "channel scope has no directory name",
        )
    })?;
    // Canonical Windows paths carry the verbatim prefix needed by native
    // directory creation for long paths. The checked parent remains pinned.
    let path = fs::canonicalize(parent)?.join(name);
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct Account {
    // TokenUser's embedded SID points into this aligned, stable allocation.
    information: Vec<usize>,
}

impl Account {
    fn current() -> Result<Self> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut length = 0;
        let result = unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length)
        };
        if result != 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || length < size_of::<TOKEN_USER>() as u32
        {
            bail!("cannot determine process account SID");
        }
        let mut information = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                information.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let account = Self { information };
        if unsafe { IsValidSid(account.sid()) } == 0 {
            bail!("invalid process account SID");
        }
        Ok(account)
    }

    fn sid(&self) -> PSID {
        unsafe { (*self.information.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn trusted(&self, sid: PSID) -> bool {
        unsafe {
            EqualSid(self.sid(), sid) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        }
    }

    fn directory_descriptor(&self) -> Result<LocalAllocation> {
        let mut text = null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let allocation = LocalAllocation(text.cast());
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })?;
        drop(allocation);
        descriptor_from_sddl(&format!(
            "O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        ))
    }
}

fn descriptor_from_sddl(sddl: &str) -> Result<LocalAllocation> {
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(LocalAllocation(descriptor))
}

fn validate_private(file: &File, account: &Account) -> Result<()> {
    let mut owner = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32).into());
    }
    let _descriptor = LocalAllocation(descriptor);
    if owner.is_null()
        || unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(account.sid(), owner) } == 0
        || dacl.is_null()
        || unsafe { IsValidAcl(dacl) } == 0
    {
        bail!("channel scope must have an account-owned non-null private DACL");
    }
    // The ACL header is copied without assuming descriptor alignment.
    let ace_count = unsafe { dacl.read_unaligned() }.AceCount;
    for index in 0..ace_count {
        let mut pointer = null_mut();
        if unsafe { GetAce(dacl, u32::from(index), &mut pointer) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let header = unsafe { pointer.cast::<ACE_HEADER>().read_unaligned() };
        match u32::from(header.AceType) {
            ACCESS_DENIED_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => {
                if usize::from(header.AceSize)
                    < offset_of!(ACCESS_ALLOWED_ACE, SidStart) + offset_of!(SID, SubAuthority)
                {
                    bail!("invalid channel scope ACL entry");
                }
                let ace = unsafe { pointer.cast::<ACCESS_ALLOWED_ACE>().read_unaligned() };
                let sid = unsafe {
                    pointer
                        .cast::<u8>()
                        .add(offset_of!(ACCESS_ALLOWED_ACE, SidStart))
                        .cast()
                };
                if unsafe { IsValidSid(sid) } == 0
                    || offset_of!(ACCESS_ALLOWED_ACE, SidStart)
                        + unsafe { GetLengthSid(sid) } as usize
                        > usize::from(header.AceSize)
                    || (ace.Mask != 0 && !account.trusted(sid))
                {
                    bail!(
                        "channel scope ACL grants access outside the process account, SYSTEM, or Administrators"
                    );
                }
            }
            _ => bail!("unsupported channel scope ACL entry"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, SetSecurityInfo,
        },
        Security::{GetSecurityDescriptorControl, GetSecurityDescriptorDacl, SE_DACL_PROTECTED},
        Storage::FileSystem::WRITE_DAC,
    };

    fn snapshot(path: &Path) -> (String, u16) {
        let file = open_directory(path).unwrap();
        let mut descriptor = null_mut();
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        let descriptor = LocalAllocation(descriptor);
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
            0
        );
        let mut text = null_mut();
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor.0,
                    SDDL_REVISION_1,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut text,
                    null_mut(),
                )
            },
            0
        );
        let _text = LocalAllocation(text.cast());
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        (
            String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap(),
            control,
        )
    }

    fn set_dacl(path: &Path, sddl: Option<&str>) {
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .unwrap();
        let descriptor = sddl.map(|sddl| descriptor_from_sddl(sddl).unwrap());
        let mut dacl = null_mut();
        if let Some(descriptor) = &descriptor {
            let mut present = 0;
            let mut defaulted = 0;
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
                },
                0
            );
        }
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    dacl,
                    null_mut(),
                )
            },
            0
        );
    }

    #[test]
    fn new_scope_uses_process_owner_and_protected_acl_without_changing_project_parent() {
        let root = tempfile::tempdir().unwrap();
        let before = snapshot(root.path());
        let scope = root.path().join("channel-state").join("plugin");
        ensure(&scope).unwrap();
        validate(&scope).unwrap();
        assert_eq!(snapshot(root.path()), before);
        for path in [scope.parent().unwrap(), scope.as_path()] {
            validate(path).unwrap();
            assert_ne!(snapshot(path).1 & SE_DACL_PROTECTED, 0);
        }
        let original = snapshot(&scope);
        ensure(&scope).unwrap();
        assert_eq!(snapshot(&scope), original);
    }

    #[test]
    fn existing_broad_or_null_dacl_is_refused_without_repair() {
        let root = tempfile::tempdir().unwrap();
        for (name, sddl) in [("broad", Some("D:P(A;;FA;;;WD)")), ("null", None)] {
            let scope = root.path().join(name);
            ensure(&scope).unwrap();
            set_dacl(&scope, sddl);
            let before = snapshot(&scope);
            assert!(ensure(&scope).is_err());
            assert!(validate(&scope).is_err());
            assert_eq!(snapshot(&scope), before);
        }
    }

    #[test]
    fn validation_never_creates_missing_owner_storage() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing").join("scope");
        assert!(validate(&missing).is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn concurrent_creators_validate_the_same_private_scope() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("new").join("scope");
        let first = scope.clone();
        let second = scope.clone();
        let first = std::thread::spawn(move || ensure(&first));
        let second = std::thread::spawn(move || ensure(&second));
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        validate(&scope).unwrap();
    }

    #[test]
    fn nested_scope_creation_supports_long_direct_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut scope = root.path().to_path_buf();
        for index in 0..4 {
            scope.push(format!("segment-{index}-{}", "x".repeat(60)));
        }
        assert!(scope.as_os_str().encode_wide().count() > 260);
        ensure(&scope).unwrap();
        validate(&scope).unwrap();
    }

    #[test]
    fn junction_scope_is_refused_before_creation_beneath_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        ensure(&target).unwrap();
        let link = root.path().join("link");
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {output:?}"
        );
        assert!(ensure(&link.join("scope")).is_err());
        assert!(validate(&link).is_err());
        assert!(!target.join("scope").exists());
        fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn protected_session_and_stream_paths_are_refused_without_creating_anything() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            ".agent-shell",
            ".AGENT-SHELL",
            "state:stream",
            ".agent-shell.",
            "scope ",
        ] {
            let path = root.path().join(name);
            assert!(ensure(&path.join("scope")).is_err());
            assert!(validate(&path).is_err());
            assert!(!path.exists());
            assert!(!root.path().join(name.trim_end_matches(['.', ' '])).exists());
        }
        // Device names are refused before any native open or creation.
        for name in ["NUL", "con.txt", "Lpt1", "COM9 .db", "lpt\u{b3}.log"] {
            assert!(ensure(&root.path().join(name).join("scope")).is_err());
            assert!(validate(&root.path().join(name)).is_err());
        }
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }
}
