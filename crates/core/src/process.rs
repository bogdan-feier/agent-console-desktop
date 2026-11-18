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

/// Stop every process whose working folder is `dir` or inside it.
///
/// An agent that starts the app it built to try it (`uvicorn ... &`) can leave it
/// running: the sandbox does not let the agent signal it, and it is in a process
/// group of its own, so stopping the agent misses it. The app runs outside the
/// sandbox and can stop it, and when the folder is about to be deleted nothing should
/// still be running there. `lsof` lists each process's working folder; without
/// `lsof` nothing is stopped.
pub async fn stop_processes_in(dir: &std::path::Path) {
    // lsof names folders by their real path (/private/var/..., not /var/...).
    let Ok(dir) = std::fs::canonicalize(dir) else { return };
    let Ok(output) =
        tokio::process::Command::new("lsof").args(["-a", "-d", "cwd", "-F", "pn"]).output().await
    else {
        return;
    };
    let mut pid = None;
    let mut inside = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(number) = line.strip_prefix('p') {
            pid = Some(number.to_string());
        } else if let (Some(path), Some(number)) = (line.strip_prefix('n'), &pid) {
            if std::path::Path::new(path).starts_with(&dir) {
                inside.push(number.clone());
            }
        }
    }
    if !inside.is_empty() {
        let _ = tokio::process::Command::new("kill").args(&inside).output().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_program_left_running_in_a_folder_is_stopped_with_it() {
        if std::process::Command::new("lsof").arg("-v").output().is_err() {
            return; // Nothing to find processes with on this machine.
        }
        let folder = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // Like an agent's `uvicorn ... &`: started in the folder and left running.
        let mut left_running =
            std::process::Command::new("sleep").arg("60").current_dir(folder.path()).spawn().unwrap();
        let mut unrelated =
            std::process::Command::new("sleep").arg("60").current_dir(elsewhere.path()).spawn().unwrap();

        stop_processes_in(folder.path()).await;

        let mut stopped = false;
        for _ in 0..50 {
            if left_running.try_wait().unwrap().is_some() {
                stopped = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(stopped, "the program in the folder was stopped");
        assert!(unrelated.try_wait().unwrap().is_none(), "a program elsewhere was left alone");
        unrelated.kill().unwrap();
        let _ = unrelated.wait();
    }
}
