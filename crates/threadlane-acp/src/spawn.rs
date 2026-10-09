//! Windows-specific adjustments for launching an agent subprocess and the
//! working directory it is given.
//!
//! Most ACP presets launch through npm shims (`npx`, `claude-code-acp`). On
//! Windows those are `.cmd` scripts, but `CreateProcess` only appends `.exe`
//! to a bare program name, so `Command::new("npx")` fails with "program not
//! found" even when `npx.cmd` is on `PATH`. Batch scripts also run under
//! `cmd.exe`, which cannot use a verbatim (`\\?\C:\…`) working directory and
//! silently falls back to the Windows directory.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Program to hand to `Command::new`: on Windows, a bare name resolves through
/// `path` and `pathext` (the child's `PATH`/`PATHEXT`); anything else, and every
/// other platform, is returned unchanged so the OS lookup applies.
pub(crate) fn resolve_program(
    command: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from(command);
    }
    resolve_windows_program(command, path, pathext, |candidate| candidate.is_file())
}

fn resolve_windows_program(
    command: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let bare = Path::new(command);
    // Paths and names that already carry an extension are left to the OS.
    if command.contains(['/', '\\']) || bare.extension().is_some() {
        return bare.to_path_buf();
    }
    let extensions = pathext
        .and_then(OsStr::to_str)
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .collect::<Vec<_>>();
    let Some(path) = path else {
        return bare.to_path_buf();
    };
    for dir in std::env::split_paths(path) {
        for extension in &extensions {
            let candidate = dir.join(format!("{command}{}", extension.to_ascii_lowercase()));
            if exists(&candidate) {
                return candidate;
            }
        }
    }
    bare.to_path_buf()
}

/// Working directory without the Windows verbatim prefix for drive paths, so
/// `cmd.exe` (which runs `.cmd` shims) keeps it. Other paths are unchanged.
pub(crate) fn simplified_cwd(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_windows_program, simplified_cwd};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    fn resolve(command: &str, existing: &[&str]) -> PathBuf {
        let existing = existing.iter().map(PathBuf::from).collect::<Vec<_>>();
        resolve_windows_program(
            command,
            Some(OsStr::new(r"C:\tools;C:\nodejs")),
            Some(OsStr::new(".COM;.EXE;.BAT;.CMD")),
            |candidate| existing.iter().any(|path| path == candidate),
        )
    }

    #[test]
    fn bare_names_find_npm_cmd_shims() {
        assert_eq!(resolve("npx", &[r"C:\nodejs\npx.cmd"]), Path::new(r"C:\nodejs\npx.cmd"));
    }

    #[test]
    fn earlier_path_entries_and_exe_win() {
        assert_eq!(
            resolve("node", &[r"C:\nodejs\node.exe", r"C:\tools\node.cmd", r"C:\tools\node.exe"]),
            Path::new(r"C:\tools\node.exe")
        );
    }

    #[test]
    fn explicit_paths_extensions_and_misses_are_unchanged() {
        assert_eq!(resolve(r"C:\bin\agent", &[r"C:\bin\agent.cmd"]), Path::new(r"C:\bin\agent"));
        assert_eq!(resolve("agent.exe", &[r"C:\tools\agent.exe"]), Path::new("agent.exe"));
        assert_eq!(resolve("missing", &[]), Path::new("missing"));
    }

    #[test]
    fn cwd_drops_only_the_drive_verbatim_prefix() {
        assert_eq!(simplified_cwd(Path::new(r"\\?\C:\work\repo")), Path::new(r"C:\work\repo"));
        assert_eq!(
            simplified_cwd(Path::new(r"\\?\UNC\server\share")),
            Path::new(r"\\?\UNC\server\share")
        );
        assert_eq!(simplified_cwd(Path::new("/home/me/repo")), Path::new("/home/me/repo"));
    }
}
