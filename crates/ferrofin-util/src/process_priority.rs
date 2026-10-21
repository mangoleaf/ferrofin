//! OS process priority: .NET's `Process.PriorityClass`, for the ffmpeg and
//! ffprobe processes Ferrofin runs in the background (probes, keyframe, image
//! and trickplay extraction, audio normalization, the Intro Skipper analysis).

/// A .NET `ProcessPriorityClass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPriority {
    /// `Idle`.
    Idle,
    /// `BelowNormal`.
    BelowNormal,
    /// `Normal`.
    Normal,
    /// `AboveNormal`.
    AboveNormal,
    /// `High`.
    High,
    /// `RealTime`.
    RealTime,
}

impl ProcessPriority {
    /// The nice value .NET sets for the class on Unix
    /// (`Process.SetPriorityClassCore`).
    #[must_use]
    pub fn nice(self) -> i32 {
        match self {
            Self::Idle => 19,
            Self::BelowNormal => 10,
            Self::Normal => 0,
            Self::AboveNormal => -6,
            Self::High => -11,
            Self::RealTime => -19,
        }
    }
}

/// A started child process whose priority can be set: a
/// `std::process::Child`, or with the `tokio` feature a
/// `tokio::process::Child`. Set it before waiting on the child: a std child
/// keeps reporting its pid once reaped, when the OS may have given that pid
/// to another process.
pub trait ChildProcess {
    /// The OS process id, or `None` when it is known to have been reaped
    /// (a tokio child; a std child cannot tell).
    fn process_id(&self) -> Option<u32>;

    /// The process handle, or `None` once the child has been reaped.
    #[cfg(windows)]
    fn process_handle(&self) -> Option<std::os::windows::io::RawHandle>;
}

impl ChildProcess for std::process::Child {
    fn process_id(&self) -> Option<u32> {
        Some(self.id())
    }

    #[cfg(windows)]
    fn process_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        use std::os::windows::io::AsRawHandle as _;
        Some(self.as_raw_handle())
    }
}

#[cfg(feature = "tokio")]
impl ChildProcess for tokio::process::Child {
    fn process_id(&self) -> Option<u32> {
        self.id()
    }

    #[cfg(windows)]
    fn process_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        self.raw_handle()
    }
}

/// `process.PriorityClass = priority` on a started child. A process that has
/// already exited has no priority left to set, which is not an error.
///
/// # Errors
///
/// The OS refusal, such as raising a priority (lowering the nice value)
/// without the privilege to.
#[cfg(unix)]
pub fn set_process_priority(
    child: &impl ChildProcess,
    priority: ProcessPriority,
) -> std::io::Result<()> {
    let Some(pid) = child.process_id() else {
        return Ok(());
    };
    // SAFETY: setpriority only reads its scalar arguments.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::id_t::from(pid), priority.nice()) } == 0
    {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // It exited before its priority could be set.
        return Ok(());
    }
    Err(error)
}

/// `process.PriorityClass = priority` on a started child: the class itself.
///
/// # Errors
///
/// The OS refusal, such as raising a priority without the privilege to.
#[cfg(windows)]
pub fn set_process_priority(
    child: &impl ChildProcess,
    priority: ProcessPriority,
) -> std::io::Result<()> {
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS, SetPriorityClass,
    };
    let Some(handle) = child.process_handle() else {
        return Ok(());
    };
    let class = match priority {
        ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
        ProcessPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
        ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        ProcessPriority::High => HIGH_PRIORITY_CLASS,
        ProcessPriority::RealTime => REALTIME_PRIORITY_CLASS,
    };
    // SAFETY: the handle is the live child's, owned by `child`.
    if unsafe { SetPriorityClass(handle.cast(), class) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Other targets have no priority to set.
///
/// # Errors
///
/// Never.
#[cfg(not(any(unix, windows)))]
pub fn set_process_priority(
    _child: &impl ChildProcess,
    _priority: ProcessPriority,
) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ProcessPriority;
    #[cfg(target_os = "linux")]
    use super::set_process_priority;
    use rstest::rstest;

    #[rstest]
    #[case(ProcessPriority::Idle, 19)]
    #[case(ProcessPriority::BelowNormal, 10)]
    #[case(ProcessPriority::Normal, 0)]
    #[case(ProcessPriority::AboveNormal, -6)]
    #[case(ProcessPriority::High, -11)]
    #[case(ProcessPriority::RealTime, -19)]
    fn each_class_maps_to_dotnets_nice_value(#[case] priority: ProcessPriority, #[case] nice: i32) {
        assert_eq!(priority.nice(), nice);
    }

    /// The nice value of process `pid` (`/proc/<pid>/stat`, field 19).
    #[cfg(target_os = "linux")]
    fn nice_of(pid: u32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
        // The command name may hold spaces; the fields after it do not.
        let after_name = &stat[stat.rfind(')').expect("comm") + 2..];
        after_name
            .split(' ')
            .nth(16)
            .expect("nice field")
            .parse()
            .expect("a number")
    }

    /// The class's nice value lands on a live child. Lowering a priority
    /// (raising the nice value) needs no privilege, so `Idle` always works.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_child_takes_the_class() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("sleep");
        let pid = child.id();
        set_process_priority(&child, ProcessPriority::Idle).expect("lowering is allowed");
        assert_eq!(nice_of(pid), 19);
        child.kill().ok();
        child.wait().ok();
    }

    /// A process that has already exited is not an error to reprioritise: its
    /// pid is gone (ESRCH).
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_process_is_not_an_error() {
        let mut child = std::process::Command::new("true").spawn().expect("true");
        child.wait().expect("reaped");
        assert!(set_process_priority(&child, ProcessPriority::BelowNormal).is_ok());
    }
}
