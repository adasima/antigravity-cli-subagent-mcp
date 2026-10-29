//! Process-tree kill for agy's children.
//!
//! `child.start_kill()` ends agy.exe only; the powershell → tool it shelled out stays
//! alive as an orphan and fights the next order for CPU/GPU/files (gen3dasset, j5).
//! On Windows agy is put in a job object with KILL_ON_JOB_CLOSE, so terminating or
//! dropping the job ends the tree. Other OSes keep the old single-process behavior.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;

    /// Armed job: dropping it (or `terminate`) kills every process in it.
    pub(crate) struct ProcessTree {
        job: HANDLE,
    }

    // A HANDLE is a plain kernel object id; safe to move between threads.
    unsafe impl Send for ProcessTree {}
    unsafe impl Sync for ProcessTree {}

    fn set_kill_on_close(job: HANDLE, on: bool) -> bool {
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            if on {
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            }
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
        }
    }

    impl ProcessTree {
        /// Best effort: `None` when the job can't be made or the process can't join
        /// (the caller then falls back to killing the process alone).
        pub(crate) fn adopt(process: HANDLE) -> Option<Self> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let me = ProcessTree { job };
                if !set_kill_on_close(job, true) || AssignProcessToJobObject(job, process) == 0 {
                    return None; // Drop closes the job
                }
                Some(me)
            }
        }

        /// Image names (`powershell.exe`, …) of the processes now in the job.
        pub(crate) fn process_names(&self) -> Vec<String> {
            use windows_sys::Win32::Foundation::FALSE;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            const CAP: usize = 256;
            // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts, then the ids.
            let mut buf = vec![0usize; 2 + CAP];
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr() as *mut _,
                    (buf.len() * std::mem::size_of::<usize>()) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Vec::new();
            }
            let header = unsafe { *(buf.as_ptr() as *const [u32; 2]) };
            let n = (header[1] as usize).min(CAP);
            let mut names = Vec::new();
            for &pid in &buf[2..2 + n] {
                unsafe {
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid as u32);
                    if h.is_null() {
                        names.push("?".to_string());
                        continue;
                    }
                    let mut path = [0u16; 520];
                    let mut len = path.len() as u32;
                    if QueryFullProcessImageNameW(h, 0, path.as_mut_ptr(), &mut len) != 0 {
                        let full = String::from_utf16_lossy(&path[..len as usize]);
                        if let Some(name) = full.rsplit(['\\', '/']).next() {
                            names.push(name.to_string());
                        }
                    }
                    CloseHandle(h);
                }
            }
            names
        }

        /// Kill every process in the tree now.
        pub(crate) fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }

        /// Normal exit: leave whatever agy started (background tasks the bridge
        /// resumes for) alone — clear the flag so closing the handle kills nothing.
        pub(crate) fn disarm(&self) {
            set_kill_on_close(self.job, false);
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub(crate) struct ProcessTree;
    impl ProcessTree {
        pub(crate) fn terminate(&self) {}
        pub(crate) fn process_names(&self) -> Vec<String> {
            Vec::new()
        }
        pub(crate) fn disarm(&self) {}
    }
}

pub(crate) use imp::ProcessTree;

#[cfg(windows)]
impl ProcessTree {
    /// Put a freshly spawned tokio child in a job. `None` if it already exited or
    /// the OS refused.
    pub(crate) fn of_child(child: &tokio::process::Child) -> Option<Self> {
        let h = child.raw_handle()?;
        Self::adopt(h as _)
    }
}

#[cfg(not(windows))]
impl ProcessTree {
    pub(crate) fn of_child(_child: &tokio::process::Child) -> Option<Self> {
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn alive(pid: u32) -> bool {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }

    /// powershell starts a long-sleeping grandchild; closing the job ends both.
    #[tokio::test]
    async fn dropping_the_job_kills_the_grandchild() {
        let script = "$p = Start-Process -PassThru -WindowStyle Hidden powershell \
            -ArgumentList '-NoProfile','-Command','Start-Sleep 120'; \
            Write-Output $p.Id; Start-Sleep 120";
        let mut child = tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let tree = ProcessTree::of_child(&child).expect("job");
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        let grand: u32 = line.trim().parse().expect("grandchild pid");
        assert!(alive(grand));
        let names = tree.process_names();
        assert!(
            names.len() >= 3
                && names
                    .iter()
                    .any(|n| n.to_lowercase().starts_with("powershell")),
            "{names:?}"
        );
        drop(tree);
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!alive(grand), "grandchild survived the job close");
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn disarmed_job_leaves_children_alone() {
        let mut child = tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep 30"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let tree = ProcessTree::of_child(&child).expect("job");
        tree.disarm();
        drop(tree);
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        assert!(alive(pid));
        let _ = child.kill().await;
    }
}
