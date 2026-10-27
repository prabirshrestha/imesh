//! Windows: the local API is a named pipe.
//!
//! Another local user can create a pipe with any name first, so the name alone proves nothing.
//! The daemon creates its pipe with an access list by SID, and a client checks that the process
//! at the other end runs as the same user, or as a service identity, before it sends anything.

use std::{
    ffi::c_void,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    ptr,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation, LookupAccountNameW,
        PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Pipes::GetNamedPipeServerProcessId,
        Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

use crate::{api::Io, net::local::Access};

/// ERROR_PIPE_BUSY: every server instance is taken for a moment.
const PIPE_BUSY: i32 = 231;

pub async fn connect(path: &Path) -> Result<Box<dyn Io>> {
    for _ in 0..50 {
        match ClientOptions::new().open(path) {
            Ok(c) => {
                check_server_owner(&c).with_context(|| format!("pipe {}", path.display()))?;
                return Ok(Box::new(c));
            }
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await
            }
            Err(e) => return Err(anyhow!("connect {}: {e} (is the imesh daemon running?)", path.display())),
        }
    }
    Err(anyhow!("connect {}: pipe busy", path.display()))
}

pub struct Listener {
    path: PathBuf,
    sddl: Vec<u16>,
    next: NamedPipeServer,
}

impl Listener {
    /// A pipe that only its owner and SYSTEM may open, with remote clients refused. With
    /// [`Access::Group`], also Administrators and the members of that local group.
    pub fn bind(path: &Path, access: Access) -> Result<Self> {
        let sddl = pipe_sddl(&access)?;
        let next = create_pipe(path, true, &sddl)
            .with_context(|| format!("create pipe {} (is another daemon running?)", path.display()))?;
        Ok(Listener { path: path.to_path_buf(), sddl, next })
    }

    pub async fn accept(&mut self) -> Result<Box<dyn Io>> {
        let connected = self.next.connect().await;
        // The pipe instance is used up either way. Make the next one first.
        let fresh = create_pipe(&self.path, false, &self.sddl)?;
        let this = std::mem::replace(&mut self.next, fresh);
        connected?;
        Ok(Box::new(this))
    }

    pub fn remove(_path: &Path) {}
}

/// Ctrl-C, or the console closing.
pub fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c().expect("listen for Ctrl-C");
    async move {
        ctrl_c.recv().await;
    }
}

pub fn sd_notify_ready() {}

/// Puts this process in a new job that lets children leave it, as sshd does for its sessions.
/// Task Scheduler runs the daemon in a job that does not, so a program that starts a background
/// copy of itself with `CREATE_BREAKAWAY_FROM_JOB` (Zed's remote server, for example) fails with
/// access denied. Jobs nest on Windows 8 and later. Best effort: a failure keeps the old job.
/// Nothing to do: Windows has no SIGPIPE, and a write to a closed pipe fails with an error.
pub fn exit_on_closed_pipe() {}

pub fn allow_job_breakaway() {
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Threading::GetCurrentProcess,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
        // The job lives while a process is in it; this handle is not needed.
        windows_sys::Win32::Foundation::CloseHandle(job);
    }
}

/// Creates a directory and its missing parents. A directory this call creates gets an access
/// list of its own, for this user, SYSTEM, and Administrators only, so its parent's list (for
/// example ProgramData's, which lets Users read) does not apply.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let me = sid_string(&user_of(current_process())?)?;
    let sddl = wide(&format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"));
    let sd = SecurityDescriptor::from_sddl(&sddl)?;
    let (mut present, mut defaulted, mut dacl) = (0, 0, ptr::null_mut());
    // SAFETY: a valid descriptor and out pointers; the ACL points into the descriptor.
    ensure!(
        unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) } != 0,
        "read an access list: {}",
        std::io::Error::last_os_error()
    );
    let path = wide(&dir.display().to_string());
    // SAFETY: a NUL-terminated path and an ACL that lives in `sd` until the call returns.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    ensure!(rc == 0, "set the access list of {}: {}", dir.display(), std::io::Error::from_raw_os_error(rc as i32));
    Ok(())
}

pub fn private_file(_: &mut std::fs::OpenOptions) {}

pub fn sync_dir(_: &Path) -> Result<()> {
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn current_process() -> HANDLE {
    // SAFETY: the pseudo handle of this process needs no closing.
    unsafe { GetCurrentProcess() }
}

/// A security descriptor from SDDL, freed on drop.
struct SecurityDescriptor(*mut c_void);

impl SecurityDescriptor {
    fn from_sddl(sddl: &[u16]) -> Result<Self> {
        let mut sd = ptr::null_mut();
        // SAFETY: a NUL-terminated string and an out pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        };
        ensure!(ok != 0, "build an access list: {}", std::io::Error::last_os_error());
        Ok(SecurityDescriptor(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// The pipe's access list: this process's user and SYSTEM, by SID rather than OWNER RIGHTS
/// (an elevated process's objects belong to Administrators). A group pipe adds Administrators
/// and the named local group.
fn pipe_sddl(access: &Access) -> Result<Vec<u16>> {
    let me = sid_string(&user_of(current_process())?)?;
    let mut sddl = format!("D:P(A;;GA;;;{me})(A;;GA;;;SY)");
    if let Access::Group(group) = access {
        let group = lookup_sid(group).with_context(|| format!("find the local group {group:?}"))?;
        sddl.push_str(&format!("(A;;GA;;;BA)(A;;GA;;;{group})"));
    }
    Ok(wide(&sddl))
}

fn create_pipe(path: &Path, first: bool, sddl: &[u16]) -> Result<NamedPipeServer> {
    let sd = SecurityDescriptor::from_sddl(sddl)?;
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    // SAFETY: `sa` and the descriptor it points to live until the call returns.
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(path, &mut sa as *mut _ as *mut c_void)
    };
    Ok(pipe?)
}

/// Accepts a pipe served by this user, or by a service identity that only an administrator can
/// create: LocalSystem, LocalService, NetworkService, or a virtual service account.
fn check_server_owner(pipe: &NamedPipeClient) -> Result<()> {
    let mut pid = 0u32;
    // SAFETY: a valid pipe handle and an out pointer.
    ensure!(
        unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) } != 0,
        "find the process that serves the pipe: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: OpenProcess returns a handle or null; it is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    ensure!(!process.is_null(), "open the process that serves the pipe: {}", std::io::Error::last_os_error());
    let theirs = user_of(process);
    // SAFETY: opened above.
    unsafe { CloseHandle(process) };
    let theirs = sid_string(&theirs?)?;
    let mine = sid_string(&user_of(current_process())?)?;
    let service = ["S-1-5-18", "S-1-5-19", "S-1-5-20"].contains(&theirs.as_str()) || theirs.starts_with("S-1-5-80-");
    if theirs != mine && !service {
        bail!("the process that serves it runs as another user ({theirs}); is someone else squatting on the name?");
    }
    Ok(())
}

/// The SID in a TOKEN_USER buffer from [`user_of`], as text.
fn sid_string(token_user: &[u64]) -> Result<String> {
    // SAFETY: the buffer holds a TOKEN_USER whose SID points into it.
    sid_to_string(unsafe { (*(token_user.as_ptr() as *const TOKEN_USER)).User.Sid })
}

fn sid_to_string(sid: *mut c_void) -> Result<String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: a valid SID; the string is freed below.
    ensure!(unsafe { ConvertSidToStringSidW(sid, &mut text) } != 0, "read a SID: {}", std::io::Error::last_os_error());
    // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
    Ok(unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text as *mut c_void);
        s
    })
}

/// The SID of a local account or group, as text.
fn lookup_sid(name: &str) -> Result<String> {
    let name = wide(name);
    let mut sid = vec![0u64; 16];
    let mut sid_len = (sid.len() * 8) as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind = 0;
    // SAFETY: buffers of the given sizes and out pointers.
    let ok = unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr() as *mut c_void,
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    ensure!(ok != 0, "{}", std::io::Error::last_os_error());
    sid_to_string(sid.as_mut_ptr() as *mut c_void)
}

/// The TOKEN_USER of a process, in a buffer that also holds the SID it points to.
fn user_of(process: HANDLE) -> Result<Vec<u64>> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: a valid process handle and an out pointer; the token is closed below.
    ensure!(
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0,
        "open a process token: {}",
        std::io::Error::last_os_error()
    );
    let mut buf = vec![0u64; 64];
    let mut len = 0u32;
    // SAFETY: the buffer is 512 bytes, enough for TOKEN_USER and any SID.
    let ok = unsafe {
        GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut len)
    };
    let err = std::io::Error::last_os_error();
    // SAFETY: opened above.
    unsafe { CloseHandle(token) };
    ensure!(ok != 0, "read a process token: {err}");
    Ok(buf)
}
