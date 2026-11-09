use std::{path::Path, process::Command};

use crate::error::{Error, Result};

/// Reads pull request data through the GitHub CLI.
pub struct Gh;

impl Gh {
    /// The pull request number for `branch` at `dir`, or `None` when the branch has no pull request.
    pub fn pr_number(dir: Option<&Path>, branch: &str) -> Result<Option<u64>> {
        let mut cmd = Command::new("gh");
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        let out = cmd
            .args(["pr", "view", branch, "--json", "number", "-q", ".number"])
            .output()?;
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            return text
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|e| Error::Command(format!("failed to parse PR number: {e}")));
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no pull requests found") {
            Ok(None)
        } else {
            Err(Error::Command(stderr.trim().to_string()))
        }
    }
}
