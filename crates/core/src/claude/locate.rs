//! Finding the `claude` program and a usable PATH.
//!
//! An app started from Finder or the Dock does not get the PATH you have in a
//! terminal; macOS gives it only `/usr/bin:/bin:/usr/sbin:/sbin`. So `claude`
//! (and tools it runs, like `git`, `uv` or `cargo`) would not be found. We ask
//! your login shell for its PATH once and use that.

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROGRAM: &str = "claude";
/// Set this to the full path of `claude` to skip the search.
pub const OVERRIDE_VARIABLE: &str = "AGENT_CONSOLE_CLAUDE";
const START: &str = "__AGENT_CONSOLE_PATH_START__";
const END: &str = "__AGENT_CONSOLE_PATH_END__";

/// Where `claude` is, the PATH to give it, and which options it understands.
#[derive(Debug, Clone)]
pub struct ClaudeLocation {
    pub program: PathBuf,
    pub path_variable: String,
    /// Every `--option` this version of `claude` lists in `--help`, or `None` if the
    /// help text could not be read. Newer options are only passed when listed here,
    /// so an older Claude Code still works, just without those savings.
    pub known_flags: Option<HashSet<String>>,
}

/// Look for `claude`. This starts a shell, so call it once and keep the answer.
pub fn locate() -> Result<ClaudeLocation, String> {
    let path_variable = combined_path(login_shell_path(), env::var("PATH").ok());

    if let Some(program) = env::var_os(OVERRIDE_VARIABLE).map(PathBuf::from) {
        return if is_file(&program) {
            let known_flags = read_known_flags(&program, &path_variable);
            Ok(ClaudeLocation { program, path_variable, known_flags })
        } else {
            Err(format!("{OVERRIDE_VARIABLE} points to {}, which does not exist.", program.display()))
        };
    }

    let candidates = env::split_paths(&path_variable).chain(well_known_dirs()).map(|dir| dir.join(PROGRAM));
    for program in candidates {
        if is_file(&program) {
            let known_flags = read_known_flags(&program, &path_variable);
            return Ok(ClaudeLocation { program, path_variable, known_flags });
        }
    }
    Err(format!(
        "Could not find the `claude` program. Install Claude Code, or set {OVERRIDE_VARIABLE} \
         to its full path."
    ))
}

/// Run `claude --help` and collect the options it lists.
fn read_known_flags(program: &Path, path_variable: &str) -> Option<HashSet<String>> {
    let output = Command::new(program)
        .arg("--help")
        .env("PATH", path_variable)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let flags = parse_flags(&String::from_utf8_lossy(&output.stdout));
    (!flags.is_empty()).then_some(flags)
}

/// Every word in `help` that looks like an option: `--` followed by letters, digits or dashes.
pub fn parse_flags(help: &str) -> HashSet<String> {
    help.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|word| word.len() > 2 && word.starts_with("--") && !word.starts_with("---"))
        .map(str::to_string)
        .collect()
}

fn is_file(path: &Path) -> bool {
    path.is_file()
}

/// Places Claude Code is commonly installed, in case the shell could not tell us.
fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".claude/local"));
        dirs.push(home.join(".npm-global/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// Ask the user's shell what PATH it would have in a terminal.
fn login_shell_path() -> Option<String> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    // -i and -l make the shell read the same startup files a terminal would
    // (.zprofile, .zshrc, ...). Startup files may print other things, so the
    // PATH is wrapped in markers and cut out of the output.
    let output = Command::new(shell)
        .args(["-ilc", &format!("printf '{START}%s{END}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    extract_between_markers(&String::from_utf8_lossy(&output.stdout))
}

fn extract_between_markers(output: &str) -> Option<String> {
    let start = output.find(START)? + START.len();
    let end = output[start..].find(END)? + start;
    let path = output[start..end].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// The shell's PATH first, then anything extra from our own PATH, no duplicates.
fn combined_path(shell_path: Option<String>, own_path: Option<String>) -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for source in [shell_path, own_path].into_iter().flatten() {
        for dir in env::split_paths(&source) {
            if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    env::join_paths(dirs).map(|joined| joined.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_cut_out_of_noisy_shell_output() {
        let output = format!("Welcome back!\n{START}/opt/homebrew/bin:/usr/bin{END}\n");
        assert_eq!(extract_between_markers(&output).as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(extract_between_markers("no markers here"), None);
        assert_eq!(extract_between_markers(&format!("{START}{END}")), None);
    }

    #[test]
    fn options_are_read_from_help_text() {
        let help = "Options:\n  --add-dir <directories...>  Extra dirs\n  \
                    --allowedTools, --allowed-tools <tools...>\n  -p, --print  Print\n  \
                    see --strict-mcp-config. ---- not an option";
        let flags = parse_flags(help);
        for expected in ["--add-dir", "--allowedTools", "--allowed-tools", "--print", "--strict-mcp-config"] {
            assert!(flags.contains(expected), "{expected} missing from {flags:?}");
        }
        assert!(!flags.contains("-p") && !flags.contains("----") && !flags.contains("--"));
        assert!(parse_flags("no options here").is_empty());
    }

    #[test]
    fn shell_path_comes_first_without_duplicates() {
        let combined = combined_path(Some("/a:/b".to_string()), Some("/b:/c".to_string()));
        assert_eq!(combined, "/a:/b:/c");
        assert_eq!(combined_path(None, Some("/x".to_string())), "/x");
    }
}
