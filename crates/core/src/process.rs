//! Stopping a program and everything it started.
//!
//! An agent runs `claude`, which runs `cargo test`, which runs a compiler. Killing
//! only the first of those leaves the rest running: they are children of a process
//! that no longer exists, and they keep the machine busy and the files locked.
//!
//! Each program the app starts is therefore put in a process group of its own
//! (`command.process_group(0)`), and killed by group rather than one at a time.

/// Kills a process group when it goes out of scope.
///
/// `Drop` is Rust's "when this value goes away, run this". Holding one of these
/// next to a child process means the group is cleaned up however the code leaves:
/// a normal return, an error, or the whole future being dropped because the user
/// pressed Stop.
pub(crate) struct Group(pub(crate) Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        let Some(group) = self.0 else { return };
        kill_group(group);
    }
}

impl Group {
    /// Let the group live on: the program finished by itself.
    pub(crate) fn finished(&mut self) {
        self.0 = None;
    }
}

/// Send SIGKILL to every process in the group.
///
/// `killpg` is the system call behind `kill -KILL -<group>`. Calling it directly
/// rather than running the `kill` program keeps this out of the hands of two
/// different shells' argument parsing, and means `Drop` does not start a process
/// of its own at the very moment the app is trying to stop one.
///
/// It needs `unsafe` because Rust cannot check that `group` is a real process
/// group; this one always is, as the process was started with `process_group(0)`.
/// The worst a stale number can do is fail, which is why the result is ignored.
#[cfg(unix)]
fn kill_group(group: u32) {
    let Ok(group) = i32::try_from(group) else { return };
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_group: u32) {}
