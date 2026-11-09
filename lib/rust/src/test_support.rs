use std::{path::Path, process::Command};

use crate::revision::Sha;

/// Runs a git command in `dir` and asserts that it succeeds. Blocks global and system git config
/// and sets a fixed identity, so the suite is hermetic.
pub(crate) fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Resolves `spec` to a [`Sha`] in `dir` via `git rev-parse`.
pub(crate) fn rev(dir: &Path, spec: &str) -> Sha {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", spec])
        .output()
        .unwrap();
    assert!(output.status.success());
    let hex = String::from_utf8(output.stdout).unwrap();
    Sha::new(git2::Oid::from_str(hex.trim()).unwrap())
}

/// Writes `content` to `file.txt` in `dir` and commits it with `message`.
pub(crate) fn commit(dir: &Path, content: &str, message: &str) {
    std::fs::write(dir.join("file.txt"), content).unwrap();
    git(dir, &["add", "file.txt"]);
    git(dir, &["commit", "-q", "-m", message]);
}
