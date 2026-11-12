//! Finding an external program on `$PATH`: ffmpeg, and the archive tool RAR
//! comics are read with.

use std::ffi::OsStr;
use std::path::PathBuf;

/// Looks up `program` on the process `$PATH`, returning the first
/// executable-file match.
///
/// A dependency-free stand-in for `which::which` (not a workspace dep). Absolute
/// or relative paths containing a separator are returned as-is if they exist.
#[must_use]
pub fn which_on_path(program: &str) -> Option<PathBuf> {
    which_in(program, std::env::var_os("PATH").as_deref())
}

/// [`which_on_path`] against an explicit `PATH` value, so the search is
/// unit-testable without mutating the process environment.
#[must_use]
pub fn which_in(program: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let paths = path_var?;
    std::env::split_paths(paths).find_map(|dir| {
        let candidate = dir.join(program);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_on_path_returns_absolute_path_verbatim_when_present() {
        // An input already containing a separator is returned as-is if it exists.
        let sh = which_on_path("sh").expect("sh must be on PATH in the test env");
        let again = which_on_path(sh.to_str().unwrap());
        assert_eq!(again, Some(sh));
    }

    #[test]
    fn which_on_path_absolute_missing_is_none() {
        assert_eq!(which_on_path("/definitely/not/here/nope"), None);
    }

    #[test]
    fn which_in_finds_program_in_given_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("mytool");
        std::fs::write(&bin, b"x").unwrap();
        let found = which_in("mytool", Some(tmp.path().as_os_str()));
        assert_eq!(found, Some(bin));
    }

    #[test]
    fn which_in_returns_none_for_empty_path() {
        assert_eq!(which_in("mytool", None), None);
    }

    #[test]
    fn which_in_missing_program_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(which_in("nope", Some(tmp.path().as_os_str())), None);
    }
}
