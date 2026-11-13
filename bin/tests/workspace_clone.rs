use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, workspace: Option<&str>, args: &[&str]) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_subspace"));
        command.args(["--json", "--no-interactive"]);
        if let Some(workspace) = workspace {
            command.args(["-P", "p", "-w", workspace]);
        }
        let output = command
            .args(args)
            .current_dir(
                workspace
                    .map(|name| self.dir.path().join("p/worktrees").join(name))
                    .unwrap_or_else(|| self.dir.path().to_path_buf()),
            )
            .env("HOME", self.dir.path().join("home"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "init.defaultBranch")
            .env("GIT_CONFIG_VALUE_0", "main")
            .env_remove("PROJECT_NAME")
            .env_remove("PROJECT_PATH")
            .env_remove("SUBSPACE_PROJECT")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn commit(&self, workspace: &str, file: &str) {
        let path = self.dir.path().join("p/worktrees").join(workspace);
        fs::write(path.join(file), file).unwrap();
        self.git(&path, &["add", file]);
        self.git(
            &path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                file,
            ],
        );
    }

    fn git(&self, path: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
}

#[test]
fn clone_preserves_source_fork_point_for_rebase() {
    let fixture = Fixture::new();
    let project = fixture.dir.path().join("p");
    fixture.run(
        None,
        &["project", "init", project.to_str().unwrap(), "--name", "p"],
    );
    fixture.commit("main", "base");
    fixture.run(Some("main"), &["branch", "parent"]);
    fixture.commit("parent", "inherited");
    fixture.run(Some("parent"), &["branch", "source"]);
    fixture.commit("source", "own");
    let clone = fixture.run(Some("source"), &["clone", "pr"]);
    assert_eq!(clone["parent"], "parent");
    fixture.run(Some("pr"), &["rebase", "main"]);
    let path = project.join("worktrees/pr");
    assert!(path.join("own").exists());
    assert!(!path.join("inherited").exists());
    let show = fixture.run(Some("pr"), &["show"]);
    assert_eq!(show["parent"]["name"], "main");
    assert!(project.join("worktrees/source/inherited").exists());
}
