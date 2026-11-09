use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    hash::{Hash, Hasher},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{
    error::{Error, Result},
    fs::try_flock_exclusive,
    metadata::Metadata,
    name::is_valid_name,
    paths,
    registry::GlobalState,
    workspace::{Workspace, validate_name},
    worktree::Worktree,
};

/// Name of the worktrees subdirectory within a project.
pub const WORKTREES_DIR: &str = "worktrees";
/// The derived half of a project's metadata, in the state directory. It is named for what it holds,
/// the workspace tree, rather than for the tool that writes it.
pub const METADATA_STATE_FILE: &str = "workspaces.json";
/// The authored half of a project's metadata. It sits in the config directory with the project's
/// other authored files, not beside the state, and is named for the tool whose settings it holds
/// rather than for the directory.
pub const METADATA_CONFIG_FILE: &str = "subspace.json";
/// The derived half of a project's metadata from version 8 to version 10, at the project root.
pub const LEGACY_ROOT_STATE_FILE: &str = ".workspaces.json";
/// The directory the derived half sat in from version 6 to version 7.
pub const LEGACY_METADATA_DIR: &str = ".subspace";
/// The derived half of a project's metadata from version 6 to version 7, within
/// [`LEGACY_METADATA_DIR`].
pub const LEGACY_METADATA_STATE_FILE: &str = "state.json";
/// The single metadata document projects used before version 6. A project that still has one is
/// unmigrated, and every metadata read refuses it.
pub const LEGACY_METADATA_FILE: &str = ".project.json";

/// How long [`Project::lock`] keeps trying before reporting the project locked.
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long [`Project::lock`] waits between attempts.
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// Validates a project name. Fails with [`Error::InvalidName`] unless the name starts with an ASCII
/// letter, digit, or underscore and continues with those characters or hyphens.
pub fn validate_project_name(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        return Err(Error::InvalidName(format!(
            "project name must match [A-Za-z0-9_][A-Za-z0-9_-]*: {name}"
        )));
    }
    Ok(())
}

/// Reports whether `path` is a project root: a directory whose `worktrees/` subdirectory holds at
/// least one valid git worktree. The content is the marker, so a project root carries nothing that
/// exists only to be found.
pub fn is_project_root(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path.join(WORKTREES_DIR)) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .any(|e| Worktree::get(&e.path()).is_some())
}

/// Returns the directory a rename to `new_name` moves `project` into: a sibling of its current
/// directory, named for the project.
///
/// A project directory sits under a parent that holds no other subspace state, so the sibling is
/// free to take the new name. A project at the filesystem root has no sibling to move to, which is
/// the one error case.
pub fn rename_target_path(project: &Project, new_name: &str) -> Result<PathBuf> {
    let parent = project.path().parent().ok_or_else(|| {
        Error::Command(format!(
            "project has no parent directory: {}",
            project.path().display()
        ))
    })?;
    Ok(parent.join(new_name))
}

/// Returns the directory a move into `dir` places `project` at: a child of `dir`, named for the
/// project, so the basename and the name stay together.
///
/// A destination inside the project moves the directory into itself, which is the one error
/// case.
pub fn move_target_path(project: &Project, dir: &Path) -> Result<PathBuf> {
    if dir.starts_with(project.path()) {
        return Err(Error::Command(format!(
            "destination is inside the project: {}",
            dir.display()
        )));
    }
    Ok(dir.join(project.name()))
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct ProjectInner {
    name: String,
    path: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

/// A project directory containing one or more workspace worktrees.
#[derive(Debug, Clone)]
pub struct Project {
    inner: Rc<ProjectInner>,
}

impl PartialEq for Project {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) || *self.inner == *other.inner
    }
}

impl Eq for Project {}

impl Hash for Project {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.hash(state);
    }
}

impl Project {
    /// Creates a project handle at an explicit path without an existence check. The config and
    /// state directories resolve through [`paths::project_config_dir`] and
    /// [`paths::project_state_dir`], keyed by the project name.
    pub fn new(name: String, path: PathBuf) -> Result<Project> {
        validate_project_name(&name)?;
        let config = paths::project_config_dir(&name)?;
        let state = paths::project_state_dir(&name)?;
        Ok(Project::with_dirs(name, path, config, state))
    }

    /// Creates a project handle with explicit config and state directories. This is the test
    /// injection point: the tests share one process, so the directories are pointed through this
    /// constructor rather than through the environment. It mirrors
    /// [`crate::registry::Registry::new`] against [`crate::registry::Registry::from_env`].
    pub fn with_dirs(name: String, path: PathBuf, config: PathBuf, state: PathBuf) -> Project {
        Project {
            inner: Rc::new(ProjectInner {
                name,
                path,
                config,
                state,
            }),
        }
    }

    /// Returns the project name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns the project root directory.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Returns the path to the `worktrees/` directory.
    pub fn worktrees_path(&self) -> PathBuf {
        self.inner.path.join(WORKTREES_DIR)
    }

    /// Returns the path to `config/subspace.json`.
    pub fn metadata_config_path(&self) -> PathBuf {
        self.config_path().join(METADATA_CONFIG_FILE)
    }

    /// Returns the path to `<state>/workspaces.json`.
    pub fn metadata_state_path(&self) -> PathBuf {
        self.state_path().join(METADATA_STATE_FILE)
    }

    /// Returns the path to the project config directory, `~/.config/subspace/projects/<name>/`.
    pub fn config_path(&self) -> PathBuf {
        self.inner.config.clone()
    }

    /// Returns the path to the project state directory, `~/.local/state/subspace/projects/<name>/`.
    pub fn state_path(&self) -> PathBuf {
        self.inner.state.clone()
    }

    /// Returns the path to `<state>/workspaces/`, the directory that holds one state directory per
    /// workspace.
    pub fn workspaces_state_path(&self) -> PathBuf {
        self.state_path().join("workspaces")
    }

    /// Returns the path to the `snapshots/` directory, which the first snapshot creates.
    pub fn snapshots_path(&self) -> PathBuf {
        self.inner.path.join(crate::snapshot::SNAPSHOTS_DIR)
    }

    /// Returns the path to `<state>/lock`.
    pub fn lock_path(&self) -> PathBuf {
        self.state_path().join("lock")
    }

    /// Returns a [`Metadata`] handle for this project's metadata documents.
    pub fn metadata(&self) -> Metadata {
        Metadata::new(
            self.metadata_config_path(),
            self.metadata_state_path(),
            vec![
                self.inner.path.join(LEGACY_ROOT_STATE_FILE),
                self.inner
                    .path
                    .join(LEGACY_METADATA_DIR)
                    .join(LEGACY_METADATA_STATE_FILE),
                self.inner.path.join(LEGACY_METADATA_FILE),
            ],
        )
    }

    /// Returns the configured branch prefix, if any.
    pub fn branch_prefix(&self) -> Result<Option<String>> {
        Ok(self.metadata().read()?.branch_prefix)
    }

    /// Lists all workspaces in this project, sorted by name.
    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        let ws_dir = self.worktrees_path();
        if !ws_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&ws_dir)?.filter_map(|entry| entry.ok()) {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            validate_name(&name)?;
            entries.push(name);
        }
        entries.sort();
        Ok(entries
            .into_iter()
            .map(|name| Workspace::new(self.clone(), name))
            .collect())
    }

    /// Returns any valid worktree within this project, for project-level git commands that need a
    /// cwd (for example `git branch -D`, `git worktree add`).
    ///
    /// Iterates workspaces in sorted order and returns the first that opens as a git worktree.
    pub fn anchor_worktree(&self) -> Result<Worktree> {
        self.workspaces()?
            .into_iter()
            .find_map(|ws| ws.worktree().ok())
            .ok_or_else(|| Error::NoWorktree(self.name().to_string()))
    }

    /// Returns the project's default workspace, if configured.
    pub fn default_workspace(&self) -> Result<Option<Workspace>> {
        let meta = self.metadata().read()?;
        match meta.default.as_deref() {
            Some(name) => Ok(Workspace::get(self, name)),
            None => Ok(None),
        }
    }

    /// Opens the project at an explicit `path`. Returns `None` when the directory has no worktrees.
    /// Unlike [`Project::get`], this does not read the registry: it is for callers that already
    /// hold the project path.
    pub fn open(name: String, path: PathBuf) -> Result<Option<Project>> {
        let project = Project::new(name, path)?;
        if is_project_root(project.path()) {
            Ok(Some(project))
        } else {
            Ok(None)
        }
    }

    /// Looks up a registered project by name. Returns `None` when it is not in the registry or has
    /// no worktrees.
    pub fn get(registry: &GlobalState, name: &str) -> Result<Option<Project>> {
        match registry.get(name) {
            Some(path) => Project::open(name.to_string(), path.to_path_buf()),
            None => Ok(None),
        }
    }

    /// Deletes a local branch only when Git finds it fully merged.
    pub fn delete_branch(&self, lock: &ProjectLock, name: &str) -> Result<()> {
        let anchor = self.anchor_worktree()?;
        project_git(&anchor, lock, ["branch", "-d", name])
    }

    /// Takes the project mutation lock and writes `holder` into the lock file.
    ///
    /// The returned [`ProjectLock`] is a capability token that mutation methods elsewhere in this
    /// crate require. Drop to release.
    ///
    /// Retries briefly before reporting the project locked. A spawned child process holds every
    /// open descriptor until it execs, this lock included, so a lock the holder already released
    /// can read as held for a moment. A lock another invocation is about to release is worth
    /// waiting out rather than failing on.
    ///
    /// Every mutation passes through here, so this is also where the state directory is created.
    pub fn lock(&self, holder: &str) -> Result<ProjectLock> {
        std::fs::create_dir_all(self.state_path())?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())?;

        let deadline = Instant::now() + LOCK_TIMEOUT;
        while !try_flock_exclusive(&file)? {
            if Instant::now() >= deadline {
                let mut existing = String::new();
                let _ = file.read_to_string(&mut existing);
                let existing = existing.trim();
                let info = if existing.is_empty() {
                    "unknown"
                } else {
                    existing
                };
                return Err(Error::ProjectLocked(info.to_string()));
            }
            std::thread::sleep(LOCK_RETRY);
        }

        file.seek(std::io::SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(holder.as_bytes())?;
        file.flush()?;
        Ok(ProjectLock { file })
    }

    /// The identity variables subspace injects into every session of this project: its name, its
    /// directory, and its config directory. The env files expand `$VAR` against these, so a global
    /// entry can name a file of whichever project the session belongs to.
    pub fn identity_env(&self) -> Vec<(String, String)> {
        vec![
            ("PROJECT_NAME".to_string(), self.name().to_string()),
            (
                "PROJECT_PATH".to_string(),
                self.path().to_string_lossy().into_owned(),
            ),
            (
                "PROJECT_CONFIG".to_string(),
                self.config_path().to_string_lossy().into_owned(),
            ),
        ]
    }

    /// Returns all registered projects that have worktrees, sorted by name.
    pub fn all(registry: &GlobalState) -> Result<Vec<Project>> {
        let mut projects = Vec::new();
        for (name, path) in registry.all() {
            if let Some(p) = Project::open(name.to_string(), path.to_path_buf())? {
                projects.push(p);
            }
        }
        Ok(projects)
    }
}

/// Creates a new project directory at `path`, with a single root workspace.
///
/// `create_root` materializes the root worktree under the project's `worktrees/` directory and
/// returns the workspace name it settled on. Creation reserves a new state directory before taking
/// the lock. A failed creation removes only directories it created.
///
/// The returned lock is still held: the caller applies configuration, registers the project in the
/// global registry, and writes any further metadata under it.
pub fn create_project(
    name: String,
    path: &Path,
    holder: &str,
    create_root: impl FnOnce(&Path, &ProjectLock) -> Result<String>,
) -> Result<(Project, ProjectLock, Workspace)> {
    let project = Project::new(name, path.to_path_buf())?;
    if project.state_path().exists() {
        return Err(Error::ProjectExists(project.name().to_string()));
    }
    if let Some(parent) = project.path().parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::create_dir(project.path()) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::ProjectExists(project.path().display().to_string()));
        }
        result => result?,
    }
    if let Err(e) = std::fs::create_dir(project.worktrees_path()) {
        let _ = std::fs::remove_dir_all(project.path());
        return Err(e.into());
    }
    let state = project.state_path();
    let reserve =
        std::fs::create_dir_all(state.parent().unwrap()).and_then(|_| std::fs::create_dir(&state));
    if let Err(e) = reserve {
        let _ = std::fs::remove_dir_all(project.path());
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(Error::ProjectExists(project.name().to_string()));
        }
        return Err(e.into());
    }
    let lock = match project.lock(holder) {
        Ok(lock) => lock,
        Err(e) => {
            let _ = std::fs::remove_dir_all(project.path());
            let _ = std::fs::remove_dir_all(&state);
            return Err(e);
        }
    };

    let root = match create_root(&project.worktrees_path(), &lock) {
        Ok(root) => root,
        Err(e) => {
            drop(lock);
            let _ = std::fs::remove_dir_all(project.path());
            let _ = std::fs::remove_dir_all(&state);
            return Err(e);
        }
    };

    if let Err(e) = project.metadata().update(&lock, |meta| {
        meta.default = Some(root.clone());
        Ok(())
    }) {
        drop(lock);
        let _ = std::fs::remove_dir_all(project.path());
        let _ = std::fs::remove_dir_all(&state);
        return Err(e);
    }

    let workspace = Workspace::new(project.clone(), root);
    Ok((project, lock, workspace))
}

fn project_git<I, S>(anchor: &Worktree, _lock: &ProjectLock, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = crate::worktree::git_command()
        .arg("-C")
        .arg(anchor.path())
        .args(args)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(Error::Command(stderr.trim().to_string()))
    }
}

/// Capability token that proves the project mutation lock is held.
///
/// Constructed by [`Project::lock`]. Mutation methods across the crate require it as an argument,
/// so holding the project lock is enforced at compile time. The lock is released on drop.
pub struct ProjectLock {
    file: File,
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = self.file.seek(std::io::SeekFrom::Start(0));
        let _ = self.file.set_len(0);
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn validate_project_name_error_states_the_project_and_allowed_set() {
        assert_eq!(
            validate_project_name("-leading").unwrap_err().to_string(),
            "project name must match [A-Za-z0-9_][A-Za-z0-9_-]*: -leading"
        );
    }

    fn temp_project(dir: &tempfile::TempDir) -> Project {
        Project::with_dirs(
            "alpha".to_string(),
            dir.path().join("alpha"),
            dir.path().join("config"),
            dir.path().join("state"),
        )
    }

    #[test]
    fn failed_creation_keeps_another_projects_state() {
        let dir = tempfile::tempdir().unwrap();
        paths::set_test_home(dir.path());
        let first = dir.path().join("first/demo");
        let second = dir.path().join("second/demo");
        let state = paths::project_state_dir("demo").unwrap();
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("marker"), "project").unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("marker"), "keep").unwrap();

        let result = create_project("demo".to_string(), &second, "clone", |_, _| {
            Err(Error::Command("clone failed".to_string()))
        });
        assert!(matches!(result, Err(Error::ProjectExists(_))));
        assert_eq!(
            std::fs::read_to_string(state.join("marker")).unwrap(),
            "keep"
        );
        assert_eq!(
            std::fs::read_to_string(first.join("marker")).unwrap(),
            "project"
        );
        assert!(!second.exists());
    }

    #[test]
    fn failed_creation_removes_only_its_own_state() {
        let dir = tempfile::tempdir().unwrap();
        paths::set_test_home(dir.path());
        let path = dir.path().join("demo");
        let state = paths::project_state_dir("demo").unwrap();

        let result = create_project("demo".to_string(), &path, "clone", |_, _| {
            Err(Error::Command("clone failed".to_string()))
        });
        assert!(matches!(result, Err(Error::Command(message)) if message == "clone failed"));
        assert!(!path.exists());
        assert!(!state.exists());
    }

    #[test]
    fn failed_metadata_write_removes_new_project_and_keeps_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        paths::set_test_home(dir.path());
        let path = dir.path().join("demo");
        let project = Project::new("demo".to_string(), path.clone()).unwrap();
        let config = project.metadata_config_path();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("marker"), "keep").unwrap();

        assert!(
            create_project("demo".to_string(), &path, "init", |_, _| {
                Ok("main".to_string())
            })
            .is_err()
        );
        assert!(!path.exists());
        assert!(!project.state_path().exists());
        assert_eq!(
            std::fs::read_to_string(config.join("marker")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn project_lock_rejects_a_second_holder_then_allows_retry() {
        let dir = tempfile::tempdir().unwrap();
        let project = temp_project(&dir);
        let first = project.lock("first holder").unwrap();
        let second = std::thread::spawn({
            let path = project.path().to_path_buf();
            let config = project.config_path().to_path_buf();
            let state = project.state_path().to_path_buf();
            move || {
                Project::with_dirs("alpha".to_string(), path, config, state)
                    .lock("second holder")
                    .map(|_| ())
            }
        })
        .join()
        .unwrap();
        assert!(matches!(second, Err(Error::ProjectLocked(holder)) if holder == "first holder"));
        drop(first);
        assert!(project.lock("second holder").is_ok());
    }

    #[test]
    fn project_lock_holder_process() {
        let Some(root) = std::env::var_os("SUBSPACE_LOCK_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let project = Project::with_dirs(
            "alpha".to_string(),
            root.join("alpha"),
            root.join("config"),
            root.join("state"),
        );
        let _lock = project.lock("child holder").unwrap();
        std::fs::write(root.join("lock-ready"), "ready").unwrap();
        let mut byte = [0];
        let _ = std::io::stdin().read(&mut byte).unwrap();
    }

    fn spawn_lock_holder(root: &Path) -> std::process::Child {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "project::tests::project_lock_holder_process"])
            .env("SUBSPACE_LOCK_TEST_ROOT", root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("lock-ready").exists() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("child did not acquire the project lock");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    #[test]
    fn project_lock_blocks_other_processes_and_recovers_after_release_or_exit() {
        let dir = tempfile::tempdir().unwrap();
        let project = temp_project(&dir);
        let mut child = spawn_lock_holder(dir.path());
        let result = project.lock("second holder");
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        assert!(matches!(result, Err(Error::ProjectLocked(holder)) if holder == "child holder"));
        assert!(project.lock("second holder").is_ok());

        std::fs::remove_file(dir.path().join("lock-ready")).unwrap();
        let mut child = spawn_lock_holder(dir.path());
        child.kill().unwrap();
        assert!(child.wait().is_ok());
        assert!(project.lock("after child exit").is_ok());
    }

    #[test]
    fn workspaces_lists_valid_directory_names_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let project = temp_project(&dir);
        for name in ["z-last", "main", "_private"] {
            std::fs::create_dir_all(project.worktrees_path().join(name)).unwrap();
        }
        std::fs::create_dir(project.worktrees_path().join(".hidden invalid")).unwrap();
        std::fs::write(project.worktrees_path().join("not-a-directory"), "").unwrap();

        let names: Vec<_> = project
            .workspaces()
            .unwrap()
            .into_iter()
            .map(|workspace| workspace.name().to_string())
            .collect();

        assert_eq!(names, ["_private", "main", "z-last"]);
    }

    #[test]
    fn workspaces_reject_invalid_legacy_names() {
        for name in ["-leading", "two words", "caf\u{e9}", "owner\\name"] {
            let dir = tempfile::tempdir().unwrap();
            let project = temp_project(&dir);
            std::fs::create_dir_all(project.worktrees_path().join("main")).unwrap();
            std::fs::create_dir(project.worktrees_path().join(name)).unwrap();

            assert!(matches!(project.workspaces(), Err(Error::InvalidName(_))));
        }
    }

    #[test]
    fn rename_target_path_is_a_sibling_of_the_current_directory() {
        let project = Project::with_dirs(
            "config".to_string(),
            PathBuf::from("/home/me/Projects/config"),
            PathBuf::from("/home/me/.config/subspace/projects/config"),
            PathBuf::from("/home/me/.local/state/subspace/projects/config"),
        );
        assert_eq!(
            rename_target_path(&project, "tide").unwrap(),
            PathBuf::from("/home/me/Projects/tide")
        );
    }

    #[test]
    fn rename_target_path_ignores_a_basename_that_differs_from_the_name() {
        let project = Project::with_dirs(
            "config".to_string(),
            PathBuf::from("/srv/dotfiles"),
            PathBuf::from("/srv/dotfiles/config"),
            PathBuf::from("/srv/dotfiles/state"),
        );
        assert_eq!(
            rename_target_path(&project, "tide").unwrap(),
            PathBuf::from("/srv/tide")
        );
    }

    #[test]
    fn move_target_path_is_a_child_of_the_destination_named_for_the_project() {
        let project = Project::with_dirs(
            "config".to_string(),
            PathBuf::from("/home/me/Projects/config"),
            PathBuf::from("/home/me/.config/subspace/projects/config"),
            PathBuf::from("/home/me/.local/state/subspace/projects/config"),
        );
        assert_eq!(
            move_target_path(&project, Path::new("/srv")).unwrap(),
            PathBuf::from("/srv/config")
        );
    }

    #[test]
    fn move_target_path_rejects_a_destination_inside_the_project() {
        let project = Project::with_dirs(
            "config".to_string(),
            PathBuf::from("/home/me/Projects/config"),
            PathBuf::from("/home/me/.config/subspace/projects/config"),
            PathBuf::from("/home/me/.local/state/subspace/projects/config"),
        );
        assert!(
            move_target_path(&project, Path::new("/home/me/Projects/config/worktrees")).is_err()
        );
        assert!(move_target_path(&project, Path::new("/home/me/Projects/config")).is_err());
    }

    #[test]
    fn rename_target_path_reports_a_project_with_no_parent() {
        let project = Project::with_dirs(
            "root".to_string(),
            PathBuf::from("/"),
            PathBuf::from("/config"),
            PathBuf::from("/state"),
        );
        assert!(rename_target_path(&project, "tide").is_err());
    }
}
