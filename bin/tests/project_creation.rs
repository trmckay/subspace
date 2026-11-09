use std::{fs, path::Path, process::Command};

use serde_json::{Value, json};
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

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    fn run(&self, args: &[&str]) -> (bool, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_subspace"))
            .args(["--json", "--no-interactive"])
            .args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
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
        let value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)));
        (output.status.success(), value)
    }

    fn init(&self, path: &Path) -> (bool, Value) {
        self.run(&["project", "init", path.to_str().unwrap(), "--name", "p"])
    }

    fn clone(&self, destination: &Path) -> (bool, Value) {
        self.run(&[
            "project",
            "clone",
            self.path("remote.git").to_str().unwrap(),
            destination.to_str().unwrap(),
            "--name",
            "p",
        ])
    }

    fn remote(&self) {
        assert!(
            Command::new("git")
                .args(["init", "-q", "--bare", "--initial-branch=main"])
                .arg(self.path("remote.git"))
                .status()
                .unwrap()
                .success()
        );
    }

    fn state(&self) -> std::path::PathBuf {
        self.path("state/subspace/projects/p")
    }

    fn registry(&self) -> std::path::PathBuf {
        self.path("state/subspace/projects.json")
    }

    fn block_registry_in_pre_hook(&self) {
        assert!(self.run(&["project", "list"]).0);
        let registry = self.registry();
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        fs::write(&registry, "{}\n").unwrap();
        let command = format!(
            "mv '{}' '{}'; mkdir '{}'",
            registry.display(),
            self.path("saved-registry").display(),
            registry.display()
        );
        let config = self.path("config/subspace/config.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            config,
            json!({"hooks": {"project_add_pre": command}}).to_string(),
        )
        .unwrap();
    }

    fn restore_registry(&self) {
        fs::remove_dir(self.registry()).unwrap();
        fs::rename(self.path("saved-registry"), self.registry()).unwrap();
        fs::write(self.path("config/subspace/config.json"), "{}").unwrap();
    }
}

#[test]
fn failed_init_registration_leaves_no_project_or_state() {
    let fixture = Fixture::new();
    fixture.block_registry_in_pre_hook();
    let path = fixture.path("p");
    assert!(!fixture.init(&path).0);
    assert!(!path.exists());
    assert!(!fixture.state().exists());
    fixture.restore_registry();
    assert!(fixture.init(&path).0);
}

#[test]
fn range_paths_leave_git_options_before_the_path_separator() {
    let fixture = Fixture::new();
    let path = fixture.path("p");
    assert!(fixture.init(&path).0);
    let worktree = path.join("worktrees/main");
    fs::write(worktree.join("tracked"), "one\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "tracked"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial"
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::write(worktree.join("tracked"), "two\n").unwrap();
    for command in ["diff", "log"] {
        let (ok, value) = fixture.run(&[
            "-P", "p", "-w", "main", command, "--path", "tracked", "--", "--stat",
        ]);
        assert!(ok, "{value}");
        let args = value["args"].as_array().unwrap();
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert!(
            args.iter().position(|arg| arg == "--stat").unwrap() < separator,
            "{value}"
        );
    }
}

#[test]
fn relative_local_clone_source_resolves_from_the_caller() {
    let fixture = Fixture::new();
    fixture.remote();
    let (ok, result) = fixture.run(&["project", "clone", "remote.git", "projects", "--name", "p"]);
    assert!(ok, "{result}");
    assert!(fixture.path("projects/p/worktrees/main").exists());
}

#[test]
fn failed_clone_registration_leaves_no_project_or_state() {
    let fixture = Fixture::new();
    fixture.remote();
    fixture.block_registry_in_pre_hook();
    let parent = fixture.path("projects");
    assert!(!fixture.clone(&parent).0);
    assert!(!parent.join("p").exists());
    assert!(!fixture.state().exists());
    fixture.restore_registry();
    assert!(fixture.clone(&parent).0);
}

#[test]
fn duplicate_init_without_state_keeps_the_registered_project() {
    let fixture = Fixture::new();
    let original = fixture.path("first");
    let extra = fixture.path("second");
    assert!(fixture.init(&original).0);
    fs::rename(fixture.state(), fixture.path("saved-state")).unwrap();
    let (ok, result) = fixture.init(&extra);
    assert!(!ok);
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("project already exists")
    );
    assert!(!extra.exists());
    assert!(!fixture.state().exists());
    assert!(fixture.path("saved-state").exists());
    assert_eq!(
        fixture.run(&["project", "list"]).1["p"],
        json!(original.canonicalize().unwrap())
    );
}

#[test]
fn duplicate_clone_without_state_keeps_the_registered_project() {
    let fixture = Fixture::new();
    fixture.remote();
    let original = fixture.path("first");
    let extra = fixture.path("other");
    assert!(fixture.init(&original).0);
    fs::rename(fixture.state(), fixture.path("saved-state")).unwrap();
    let (ok, result) = fixture.clone(&extra);
    assert!(!ok);
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("project already exists")
    );
    assert!(!extra.join("p").exists());
    assert!(!fixture.state().exists());
    assert!(fixture.path("saved-state").exists());
    assert_eq!(
        fixture.run(&["project", "list"]).1["p"],
        json!(original.canonicalize().unwrap())
    );
}
