//! Fake executables for unit and integration tests.

use std::path::Path;

/// Writes an executable script that a test can spawn right away.
///
/// Writing the file in this process would leave a writable descriptor open
/// for a moment. A child forked by a parallel test inherits it until that
/// child execs, and on Linux exec of a file open for writing fails with
/// ETXTBSY. The body goes to a side file, and `cp` creates the executable,
/// so the only writer of the executable is a separate process.
#[cfg(unix)]
pub fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let source = path.with_extension("src");
    std::fs::write(&source, body).unwrap();
    let status = std::process::Command::new("cp")
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "cp {} failed", source.display());
    std::fs::remove_file(&source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
