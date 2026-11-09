use std::path::{Path, PathBuf};

use crate::{
    error::{Error, Result},
    project::Project,
    revision::{Revision, Sha},
    workspace::Workspace,
};

/// Name of the snapshots subdirectory within a project.
pub const SNAPSHOTS_DIR: &str = "snapshots";

/// The extension every snapshot file carries.
const EXTENSION: &str = ".tar.gz";

/// How many hex digits of the commit sha a snapshot's name holds.
const SHA_LEN: usize = 12;

/// A snapshot file: a gzipped tar of one commit's tree, named for the workspace it came from and
/// that commit.
///
/// The name is the whole record. There is no metadata file and no packed objects, so a snapshot
/// restores only where its commit is already in the project's repository.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The workspace the snapshot was taken from.
    workspace: String,
    /// The abbreviated commit sha the snapshot holds.
    sha: String,
    /// The snapshot file.
    path: PathBuf,
}

impl Snapshot {
    /// Reads a snapshot's workspace and commit out of `path`'s file name.
    ///
    /// The file itself is not opened, so this also names a snapshot that is not yet written.
    pub fn open(path: &Path) -> Result<Snapshot> {
        let invalid = || Error::InvalidSnapshotName(path.to_string_lossy().into_owned());
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(invalid)?;
        let stem = name.strip_suffix(EXTENSION).ok_or_else(invalid)?;
        let (workspace, sha) = stem.rsplit_once('-').ok_or_else(invalid)?;
        if workspace.is_empty()
            || sha.len() != SHA_LEN
            || !sha.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        Ok(Snapshot {
            workspace: workspace.to_string(),
            sha: sha.to_string(),
            path: path.to_path_buf(),
        })
    }

    /// The workspace the snapshot was taken from.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// The abbreviated sha of the commit the snapshot holds.
    pub fn sha(&self) -> &str {
        &self.sha
    }

    /// The snapshot file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory every path in the archive sits below, which is also the file name without its
    /// extension.
    pub fn prefix(&self) -> String {
        format!("{}-{}", self.workspace, self.sha)
    }

    /// The snapshot file's size in bytes.
    pub fn size(&self) -> Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    /// Resolves the snapshot's commit in `project`. Fails when the project's repository does not
    /// have it. A snapshot carries no objects of its own, so this is what makes a restore possible.
    pub fn commit(&self, project: &Project) -> Result<Sha> {
        project
            .anchor_worktree()?
            .resolve_ref(&self.sha)
            .map_err(|error| Error::SnapshotCommitMissing {
                file: self
                    .path
                    .file_name()
                    .unwrap_or(self.path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                sha: self.sha.clone(),
                // git2's own `Display` appends its class and code, which say nothing to a user
                // reading a snapshot error.
                cause: match &error {
                    Error::Git(git) => git.message().to_string(),
                    other => other.to_string(),
                },
            })
    }
}

/// Archives the commit `sha` in `workspace` and writes it to the project's snapshots directory. The
/// caller resolves the commit, so a ref becomes a commit in one place only.
///
/// The archive holds the commit's tree and nothing else: no staged changes, no working-tree
/// changes, and no untracked files. It is written to a temporary name and moved into place, so a
/// failed run leaves no snapshot behind.
pub fn create(workspace: &Workspace, sha: &Sha) -> Result<Snapshot> {
    let worktree = workspace.worktree()?;
    let directory = workspace.project().snapshots_path();
    let hex = sha.sha();
    let name = format!("{}-{}{EXTENSION}", workspace.name(), &hex[..SHA_LEN]);
    let snapshot = Snapshot::open(&directory.join(name))?;

    if snapshot.path.exists() {
        return Err(Error::SnapshotExists(
            snapshot.path.to_string_lossy().into_owned(),
        ));
    }

    std::fs::create_dir_all(&directory)?;
    let mut partial = snapshot.path.clone().into_os_string();
    partial.push(".part");
    let partial = PathBuf::from(partial);

    match worktree.archive(sha, &snapshot.prefix(), &partial) {
        Ok(()) => {
            std::fs::rename(&partial, &snapshot.path)?;
            Ok(snapshot)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

/// The project's snapshots, sorted by file name. An absent snapshots directory is an empty list,
/// and a file that does not parse as a snapshot is skipped.
pub fn list(project: &Project) -> Result<Vec<Snapshot>> {
    let directory = project.snapshots_path();
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut names: Vec<PathBuf> = std::fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    names.sort();
    Ok(names
        .iter()
        .filter_map(|path| Snapshot::open(path).ok())
        .collect())
}

/// Resolves `file` to a snapshot: a path as given, or a bare file name in `project`'s snapshots
/// directory.
pub fn find(project: &Project, file: &str) -> Result<Snapshot> {
    let given = PathBuf::from(file);
    if given.is_file() {
        return Snapshot::open(&given);
    }
    let in_project = project.snapshots_path().join(file);
    if in_project.is_file() {
        return Snapshot::open(&in_project);
    }
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no snapshot at {file}"),
    )))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::test_support::{commit, git};

    /// Builds a project with two workspaces at different commits: `main`, which is also the anchor
    /// worktree, and `other`. The two must differ, or a resolution taken from the wrong worktree
    /// cannot be detected.
    fn seed_project(directory: &Path, name: &str) -> Project {
        let project = Project::with_dirs(
            name.to_string(),
            directory.join(name),
            directory.join(name).join("config"),
            directory.join(name).join("state"),
        );
        let worktrees = project.worktrees_path();
        std::fs::create_dir_all(&worktrees).unwrap();
        let main = worktrees.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        commit(&main, "a\n", "initial");
        git(&main, &["worktree", "add", "-q", "-b", "other", "../other"]);
        commit(&worktrees.join("other"), "other\n", "other work");
        project
    }

    /// The workspace `name` in `project`, with its HEAD.
    fn workspace_at_head(project: &Project, name: &str) -> (Workspace, Sha) {
        let workspace = Workspace::new(project.clone(), name.to_string());
        let head = workspace.worktree().unwrap().resolve_ref("HEAD").unwrap();
        (workspace, head)
    }

    /// Reads `file.txt` out of an archive, which says which commit it holds.
    fn archived_file(snapshot: &Snapshot) -> String {
        let output = Command::new("tar")
            .arg("-xzOf")
            .arg(snapshot.path())
            .arg(format!("{}/file.txt", snapshot.prefix()))
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    /// Lists an archive's entries.
    fn entries(path: &Path) -> Vec<String> {
        let output = Command::new("tar").arg("-tzf").arg(path).output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| line.trim_end_matches('/').to_string())
            .collect()
    }

    #[test]
    fn create_writes_the_commit_below_one_directory() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (workspace, head) = workspace_at_head(&project, "main");
        std::fs::write(project.worktrees_path().join("main/loose.txt"), "b\n").unwrap();

        let snapshot = create(&workspace, &head).unwrap();

        assert_eq!(snapshot.workspace(), "main");
        assert_eq!(snapshot.sha(), &head.sha()[..SHA_LEN]);
        assert_eq!(
            snapshot.path(),
            project
                .snapshots_path()
                .join(format!("main-{}.tar.gz", &head.sha()[..SHA_LEN]))
        );
        assert_eq!(
            entries(snapshot.path()),
            vec![snapshot.prefix(), format!("{}/file.txt", snapshot.prefix())],
            "the archive holds the commit only, below its own directory"
        );
    }

    #[test]
    fn create_archives_the_named_revision() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (workspace, first) = workspace_at_head(&project, "main");
        let main = project.worktrees_path().join("main");
        commit(&main, "b\n", "second");

        let snapshot = create(&workspace, &first).unwrap();

        assert_eq!(snapshot.sha(), &first.sha()[..SHA_LEN]);
        assert_eq!(archived_file(&snapshot), "a\n");
    }

    #[test]
    fn create_archives_the_scoped_workspace_commit() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (_, main_head) = workspace_at_head(&project, "main");
        let (other, other_head) = workspace_at_head(&project, "other");
        assert_ne!(other_head, main_head, "the workspaces must disagree");

        let snapshot = create(&other, &other_head).unwrap();

        assert_eq!(snapshot.workspace(), "other");
        assert_eq!(snapshot.sha(), &other_head.sha()[..SHA_LEN]);
        assert_eq!(
            archived_file(&snapshot),
            "other\n",
            "the archive holds the scoped workspace's commit, not the root's"
        );
    }

    #[test]
    fn create_archives_the_given_commit_and_resolves_nothing() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (_, main_head) = workspace_at_head(&project, "main");
        let (other, _) = workspace_at_head(&project, "other");

        let snapshot = create(&other, &main_head).unwrap();

        assert_eq!(snapshot.sha(), &main_head.sha()[..SHA_LEN]);
        assert_eq!(
            archived_file(&snapshot),
            "a\n",
            "the given commit is archived, so nothing re-resolves a ref here"
        );
    }

    #[test]
    fn create_refuses_to_overwrite_the_same_commit() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (workspace, head) = workspace_at_head(&project, "main");

        let first = create(&workspace, &head).unwrap();
        let second = create(&workspace, &head);

        assert!(matches!(second, Err(Error::SnapshotExists(_))));
        assert!(first.path().exists());
    }

    #[test]
    fn open_reads_the_workspace_and_commit_from_the_name() {
        let snapshot = Snapshot::open(Path::new("/s/fix-the-parser-0123456789ab.tar.gz")).unwrap();

        assert_eq!(snapshot.workspace(), "fix-the-parser");
        assert_eq!(snapshot.sha(), "0123456789ab");
        assert_eq!(snapshot.prefix(), "fix-the-parser-0123456789ab");
    }

    #[test]
    fn open_rejects_a_name_that_is_not_a_snapshot() {
        for name in [
            "main.tar.gz",
            "main-0123456789ab.tar",
            "main-nothex456789.tar.gz",
            "main-0123456789.tar.gz",
            "-0123456789ab.tar.gz",
        ] {
            assert!(
                matches!(
                    Snapshot::open(Path::new(name)),
                    Err(Error::InvalidSnapshotName(_))
                ),
                "{name} must not parse as a snapshot"
            );
        }
    }

    #[test]
    fn list_reports_the_snapshots_and_skips_other_files() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (workspace, head) = workspace_at_head(&project, "main");
        let main = project.worktrees_path().join("main");
        create(&workspace, &head).unwrap();
        commit(&main, "b\n", "second");
        let advanced = workspace.worktree().unwrap().resolve_ref("HEAD").unwrap();
        let second = create(&workspace, &advanced).unwrap();
        std::fs::write(project.snapshots_path().join("notes.txt"), "x").unwrap();

        let snapshots = list(&project).unwrap();

        assert_eq!(snapshots.len(), 2);
        assert!(snapshots.iter().any(|s| s.sha() == second.sha()));
        assert!(second.size().unwrap() > 0);
    }

    #[test]
    fn list_is_empty_without_a_snapshots_directory() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");

        assert!(list(&project).unwrap().is_empty());
    }

    #[test]
    fn commit_resolves_a_snapshot_the_project_still_has() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (workspace, head) = workspace_at_head(&project, "main");
        let snapshot = create(&workspace, &head).unwrap();

        assert_eq!(snapshot.commit(&project).unwrap(), head);
    }

    #[test]
    fn restore_reads_the_commit_and_the_name_out_of_a_snapshot() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (other, head) = workspace_at_head(&project, "other");
        let snapshot = create(&other, &head).unwrap();
        let name = snapshot.path().file_name().unwrap().to_str().unwrap();

        // The steps a restore takes before it hands off to `add`: find the file, then resolve the
        // commit and the default workspace name it records.
        let found = find(&project, name).unwrap();

        assert_eq!(found.commit(&project).unwrap(), head);
        assert_eq!(found.workspace(), "other");
    }

    #[test]
    fn commit_reports_a_snapshot_the_project_never_had() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let snapshot = Snapshot::open(Path::new("/s/main-0123456789ab.tar.gz")).unwrap();

        assert!(matches!(
            snapshot.commit(&project),
            Err(Error::SnapshotCommitMissing { .. })
        ));
    }

    #[test]
    fn find_takes_a_path_or_a_bare_name() {
        let temp = TempDir::new().unwrap();
        let project = seed_project(temp.path(), "demo");
        let (workspace, head) = workspace_at_head(&project, "main");
        let snapshot = create(&workspace, &head).unwrap();
        let name = snapshot.path().file_name().unwrap().to_str().unwrap();

        assert_eq!(
            find(&project, snapshot.path().to_str().unwrap())
                .unwrap()
                .sha(),
            snapshot.sha()
        );
        assert_eq!(find(&project, name).unwrap().sha(), snapshot.sha());
        assert!(find(&project, "main-0123456789ab.tar.gz").is_err());
    }
}
