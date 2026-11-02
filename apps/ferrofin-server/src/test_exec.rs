//! Test-only executable stubs that are safe to spawn under `cargo test`, whose
//! tests share one process and fork concurrently.

use std::path::Path;

/// Writes an executable `#!/bin/sh` stub at `path` running `body`, and
/// returns once it can be executed. A test thread that forks while the
/// file is open for writing hands the child that write handle until the
/// child execs, and exec'ing the stub meanwhile fails `ETXTBSY`. One
/// successful run (`--ready`, which exits before `body`) proves no handle
/// is left, and none can appear once this process has closed its own.
pub(crate) fn stub_script(path: &Path, body: &str) {
    std::fs::write(
        path,
        format!("#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\n{body}"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for _ in 0..200 {
        match std::process::Command::new(path).arg("--ready").status() {
            Ok(status) => {
                assert!(status.success(), "{}: {status}", path.display());
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }
    panic!("{} stayed busy", path.display());
}
