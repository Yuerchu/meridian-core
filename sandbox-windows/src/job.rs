// Ported from Codex (Apache-2.0): codex-rs/utils/pty/src/win/job.rs
// Changes: winapi 0.3 -> windows-sys 0.52, raw HANDLE + Drop instead of
// filedescriptor::OwnedHandle, explicit exit code on terminate.

use std::io;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;

/// Owns a Windows Job Object used to terminate a spawned process tree.
pub struct JobObject {
    handle: HANDLE,
    // A mutex makes the state check, Job Object API call, and state update
    // atomic with respect to concurrent preserve and terminate requests.
    preserve_descendants: Mutex<bool>,
}

// The raw HANDLE is only used through the Job Object APIs, which are
// documented as safe for cross-thread use.
unsafe impl Send for JobObject {}
unsafe impl Sync for JobObject {}

impl JobObject {
    /// Creates a Job Object configured to terminate all members when its last
    /// handle closes.
    pub fn create() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle == 0 {
            return Err(io::Error::last_os_error());
        }
        if let Err(err) = Self::set_limit_flags(
            handle,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        ) {
            unsafe { CloseHandle(handle) };
            return Err(err);
        }
        Ok(Self {
            handle,
            preserve_descendants: Mutex::new(false),
        })
    }

    fn set_limit_flags(handle: HANDLE, flags: u32) -> io::Result<()> {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn as_handle(&self) -> HANDLE {
        self.handle
    }

    /// Assigns a running process to this job.
    ///
    /// Assignment is not retroactive: descendants created before this call
    /// completes are not guaranteed to become members of the job. Prefer
    /// attaching the job at process creation via
    /// `ProcThreadAttributeList::set_job`.
    pub fn assign_process(&self, process_handle: HANDLE) -> io::Result<()> {
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Allows contained descendants to keep running after the root exits
    /// normally.
    ///
    /// This disables both explicit job termination and kill-on-close for this
    /// object. Calls race safely with [`Self::terminate`]: whichever operation
    /// acquires the state lock first determines whether the process tree is
    /// preserved or terminated.
    pub fn preserve_descendants(&self) -> io::Result<()> {
        let mut preserve_descendants = self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *preserve_descendants {
            return Ok(());
        }

        Self::set_limit_flags(self.handle, JOB_OBJECT_LIMIT_BREAKAWAY_OK)?;
        *preserve_descendants = true;
        Ok(())
    }

    /// Terminates every process currently assigned to the job.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        let preserve_descendants = self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *preserve_descendants {
            return Ok(());
        }

        let terminated = unsafe { TerminateJobObject(self.handle, exit_code) };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}
