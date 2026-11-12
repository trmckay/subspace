use std::{
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use git2::{BranchType, Repository, StatusOptions, SubmoduleIgnore, SubmoduleStatus};

use crate::{
    client::ClientSettings,
    error::Result,
    git::{GitFileStatus, GitStatus},
    project::ProjectLock,
    revision::{BinaryPatch, Branch, Head, Revision, Sha, StartPoint},
};

/// A git worktree.
pub struct Worktree {
    path: PathBuf,
    repo: Repository,
}

/// A git operation that is currently in progress in a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgressOperation {
    /// A rebase is in progress, with step progress when the counters are readable.
    Rebase {
        /// Step progress, or `None` when the rebase counters cannot be read.
        steps: Option<RebaseSteps>,
    },
    /// A merge is in progress.
    Merge,
    /// A cherry-pick is in progress.
    CherryPick,
    /// A revert is in progress.
    Revert,
}

/// Progress through an in-progress rebase: `done` steps applied of `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebaseSteps {
    /// Number of steps already applied.
    pub done: usize,
    /// Total number of steps in the rebase.
    pub total: usize,
}

/// Commit counts of the current branch relative to its upstream tracking branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamStatus {
    /// Commits on the local branch not yet on the upstream.
    pub ahead: usize,
    /// Commits on the upstream not yet on the local branch.
    pub behind: usize,
}

impl Worktree {
    /// Returns the filesystem path of this worktree.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens the git repository at `path`. Returns `None` when the path is not a valid git
    /// worktree.
    pub fn get(path: &Path) -> Option<Worktree> {
        Repository::open(path).ok().map(|repo| Worktree {
            path: path.to_path_buf(),
            repo,
        })
    }

    /// Initializes a fresh repository at `path` with no commits and returns the worktree. HEAD
    /// names the branch from the caller's `init.defaultBranch` configuration, but that branch does
    /// not exist yet.
    pub fn init_empty(path: &Path) -> Result<Worktree> {
        std::fs::create_dir_all(path)?;
        git_run(path, [OsStr::new("init")])?;
        Worktree::get(path)
            .ok_or_else(|| crate::error::Error::NotAWorktree(path.to_string_lossy().into_owned()))
    }

    /// Initializes a fresh repository at `path` with an empty initial commit and returns the
    /// worktree.
    ///
    /// The initial commit births the default branch, so its name resolves. The branch name follows
    /// the caller's `init.defaultBranch` git configuration.
    pub fn init(path: &Path) -> Result<Worktree> {
        let worktree = Worktree::init_empty(path)?;
        git_run(
            path,
            [
                OsStr::new("commit"),
                OsStr::new("--allow-empty"),
                OsStr::new("-m"),
                OsStr::new("Initial commit"),
            ],
        )?;
        Ok(worktree)
    }

    /// Resolves any revspec (branch, tag, SHA, `HEAD`) to a [`Sha`].
    pub fn resolve_ref(&self, spec: &str) -> Result<Sha> {
        let commit = self.repo.revparse_single(spec)?.peel_to_commit()?;
        Ok(Sha::new(commit.id()))
    }

    /// Looks up a branch by exact name and resolves it to a [`Branch`]. Returns `None` when no such
    /// branch exists.
    fn find_branch(&self, name: &str, branch_type: BranchType) -> Result<Option<Branch>> {
        let branch = match self.repo.find_branch(name, branch_type) {
            Ok(branch) => branch,
            Err(_) => return Ok(None),
        };
        let Some(name) = branch.name()? else {
            return Ok(None);
        };
        let sha = Sha::new(branch.get().peel_to_commit()?.id());
        Ok(Some(Branch::new(name.to_string(), sha)))
    }

    /// Resolves a remote-tracking branch: an exact `<remote>/<branch>` ref, or a bare `<branch>`
    /// name that exists in exactly one remote (git's DWIM). An error when a bare name matches more
    /// than one remote.
    fn find_remote_branch(&self, spec: &str) -> Result<Option<Branch>> {
        if let Some(branch) = self.find_branch(spec, BranchType::Remote)? {
            return Ok(Some(branch));
        }
        if spec == "HEAD" {
            return Ok(None);
        }
        let mut matches = Vec::new();
        let remotes = self.repo.remotes()?;
        for remote in remotes.iter() {
            let Ok(Some(remote)) = remote else { continue };
            if let Some(branch) =
                self.find_branch(&format!("{remote}/{spec}"), BranchType::Remote)?
            {
                matches.push(branch);
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(crate::error::Error::AmbiguousRemoteBranch(spec.to_string())),
        }
    }

    /// Resolves a revspec to an existing local branch, an existing remote-tracking branch
    /// (git-style DWIM of a bare branch name included), or otherwise the bare commit it points at.
    pub fn resolve_start(&self, spec: &str) -> Result<StartPoint> {
        if let Some(branch) = self.find_branch(spec, BranchType::Local)? {
            return Ok(StartPoint::Branch(branch));
        }
        if let Some(branch) = self.find_remote_branch(spec)? {
            return Ok(StartPoint::RemoteBranch(branch));
        }
        Ok(StartPoint::Commit(self.resolve_ref(spec)?))
    }

    /// Returns the current HEAD as a [`Head`]: a branch or a detached SHA.
    pub fn head(&self) -> Result<Head> {
        let reference = self.repo.head()?;
        let sha = Sha::new(reference.peel_to_commit()?.id());
        if reference.is_branch() {
            let name = reference.shorthand()?;
            Ok(Head::Branch(Branch::new(name.to_string(), sha)))
        } else {
            Ok(Head::Detached(sha))
        }
    }

    /// Returns the index and worktree file status for this repository.
    pub fn status(&self) -> Result<GitStatus> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true);

        let statuses = self.repo.statuses(Some(&mut opts))?;
        let mut index = Vec::new();
        let mut worktree = Vec::new();

        for entry in statuses.iter() {
            let Ok(p) = entry.path() else { continue };
            let s = entry.status();

            if s.is_conflicted() {
                worktree.push((PathBuf::from(p), GitFileStatus::Unmerged));
                continue;
            }

            if let Some(status) = worktree_file_status(s) {
                let path = entry
                    .index_to_workdir()
                    .and_then(|delta| delta.new_file().path())
                    .unwrap_or_else(|| Path::new(p));
                worktree.push((PathBuf::from(path), status));
            }
            if let Some(status) = index_file_status(s) {
                let path = entry
                    .head_to_index()
                    .and_then(|delta| delta.new_file().path())
                    .unwrap_or_else(|| Path::new(p));
                index.push((PathBuf::from(path), status));
            }
        }

        Ok(GitStatus { index, worktree })
    }

    /// Returns the number of commits reachable from `tip` but not from `base`. Drops any whose
    /// patches are already present on `base` (cherry-pick equivalents).
    pub fn count_unique_revisions(
        &self,
        base: &impl Revision,
        tip: &impl Revision,
    ) -> Result<usize> {
        let base_sha = base.sha();
        let tip_sha = tip.sha();
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["rev-list", "--count", "--cherry-pick", "--right-only"])
            .arg(format!("{base_sha}...{tip_sha}"))
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let count = stdout.trim().parse().map_err(|e| {
            crate::error::Error::Command(format!("failed to parse rev-list count: {e}"))
        })?;
        Ok(count)
    }

    /// Returns whether `ancestor` is an ancestor of `descendant` (`git merge-base --is-ancestor`).
    /// Equal revisions count as ancestors.
    pub fn is_ancestor(
        &self,
        ancestor: &impl Revision,
        descendant: &impl Revision,
    ) -> Result<bool> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args([
                "merge-base",
                "--is-ancestor",
                &ancestor.sha(),
                &descendant.sha(),
            ])
            .output()?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(crate::error::Error::Command(stderr.trim().to_string()))
            }
        }
    }

    /// Returns the best common ancestor of `a` and `b` (`git merge-base`). Fails with
    /// [`Error::NoCommonAncestor`](crate::error::Error::NoCommonAncestor) when the revisions share
    /// no history.
    pub fn merge_base(&self, a: &impl Revision, b: &impl Revision) -> Result<Sha> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["merge-base", &a.sha(), &b.sha()])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            if output.status.code() == Some(1) && stderr.is_empty() {
                return Err(crate::error::Error::NoCommonAncestor {
                    a: a.sha(),
                    b: b.sha(),
                });
            }
            return Err(crate::error::Error::Command(stderr.to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Sha::new(git2::Oid::from_str(stdout.trim())?))
    }

    /// Lists commits reachable from `reachable_from` but not from `excluding`, oldest first (`git
    /// rev-list --reverse reachable_from ^excluding`, that is the `excluding..reachable_from`
    /// range).
    pub fn commits_only_in(
        &self,
        reachable_from: &impl Revision,
        excluding: &impl Revision,
    ) -> Result<Vec<Sha>> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["rev-list", "--reverse", &reachable_from.sha()])
            .arg(format!("^{}", excluding.sha()))
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .split_whitespace()
            .map(|s| Ok(Sha::new(git2::Oid::from_str(s)?)))
            .collect()
    }

    /// Runs `git diff`, with an optional revision or range. Inherits stdio.
    pub fn diff(&self, rev: Option<&str>, args: &[String]) -> Result<std::process::ExitStatus> {
        let mut cmd = git_command();
        cmd.arg("-C").arg(&self.path).arg("diff");
        if let Some(rev) = rev {
            cmd.arg(rev);
        }
        cmd.args(args).status().map_err(Into::into)
    }

    /// Runs `git log`, with an optional revision or range. Inherits stdio.
    pub fn log(&self, rev: Option<&str>, args: &[String]) -> Result<std::process::ExitStatus> {
        let mut cmd = git_command();
        cmd.arg("-C").arg(&self.path).arg("log");
        if let Some(rev) = rev {
            cmd.arg(rev);
        }
        cmd.args(args).status().map_err(Into::into)
    }

    /// Runs `git diff` and returns its stdout with its status. It inherits stderr, so git
    /// diagnostics reach the human stream.
    pub fn diff_captured(&self, rev: Option<&str>, args: &[String]) -> Result<CapturedGit> {
        git_capture(&self.path, "diff", rev, args)
    }

    /// Runs `git log` and returns its stdout with its status. It inherits stderr, so git
    /// diagnostics reach the human stream.
    pub fn log_captured(&self, rev: Option<&str>, args: &[String]) -> Result<CapturedGit> {
        git_capture(&self.path, "log", rev, args)
    }

    /// Removes a clean worktree, or any worktree when `force` is set. Consumes `self` to prevent further use.
    pub fn remove_worktree(self, _lock: &ProjectLock, force: bool) -> Result<()> {
        if !force {
            let mut opts = StatusOptions::new();
            opts.include_untracked(true).recurse_untracked_dirs(true);
            let dirty = !self.repo.statuses(Some(&mut opts))?.is_empty();
            let submodule_changes = SubmoduleStatus::INDEX_ADDED
                | SubmoduleStatus::INDEX_DELETED
                | SubmoduleStatus::INDEX_MODIFIED
                | SubmoduleStatus::WD_ADDED
                | SubmoduleStatus::WD_DELETED
                | SubmoduleStatus::WD_MODIFIED
                | SubmoduleStatus::WD_INDEX_MODIFIED
                | SubmoduleStatus::WD_WD_MODIFIED
                | SubmoduleStatus::WD_UNTRACKED;
            let mut dirty_submodule = false;
            for submodule in self.repo.submodules()? {
                if self
                    .repo
                    .submodule_status(submodule.name()?, SubmoduleIgnore::None)?
                    .intersects(submodule_changes)
                {
                    dirty_submodule = true;
                    break;
                }
            }
            if dirty || dirty_submodule {
                return Err(crate::error::Error::Command(
                    "worktree is not clean".to_string(),
                ));
            }
        }
        let mut args = vec![
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
        ];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(self.path.as_os_str());
        git_run(&self.path, args)
    }

    /// Runs `git worktree repair` to fix broken worktree links.
    pub fn repair(&self, _lock: &ProjectLock) -> Result<()> {
        git_run(&self.path, ["worktree", "repair"])
    }

    /// Runs `git worktree repair <paths...>` to fix the gitdir links of the given worktrees after
    /// they or the repository moved. A no-op when `paths` is empty.
    pub fn repair_paths(&self, _lock: &ProjectLock, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&OsStr> = vec![OsStr::new("worktree"), OsStr::new("repair")];
        args.extend(paths.iter().map(|p| p.as_os_str()));
        git_run(&self.path, args)
    }

    /// Runs `git worktree prune`, which drops administrative entries for worktrees whose
    /// directories no longer exist.
    pub fn prune_worktrees(&self, _lock: &ProjectLock) -> Result<()> {
        git_run(&self.path, ["worktree", "prune"])
    }

    /// Whether the repository holds an administrative entry for a linked worktree named `name`.
    pub fn has_worktree_link(&self, name: &str) -> bool {
        self.repo.commondir().join("worktrees").join(name).is_dir()
    }

    /// Whether a local branch named `name` exists.
    pub fn has_branch(&self, name: &str) -> bool {
        self.repo.find_branch(name, BranchType::Local).is_ok()
    }

    /// Returns the name of the worktree that holds `branch` checked out (an administrative entry
    /// name, or the main worktree's directory name), or `None` when no worktree holds it. Reads the
    /// HEAD files directly, so it sees every worktree of the repository, not just this handle.
    pub fn branch_holder(&self, branch: &str) -> Option<String> {
        let common = self.repo.commondir().to_path_buf();
        let target = format!("ref: refs/heads/{branch}");
        let holds = |p: PathBuf| std::fs::read_to_string(p).is_ok_and(|head| head.trim() == target);
        if holds(common.join("HEAD")) {
            return common
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned());
        }
        for entry in std::fs::read_dir(common.join("worktrees")).ok()?.flatten() {
            if holds(entry.path().join("HEAD")) {
                return Some(entry.file_name().to_string_lossy().into_owned());
            }
        }
        None
    }

    /// Recreates the deleted administrative entry for the linked worktree at `path`, points its
    /// HEAD at `branch`, and links `<path>/.git` back to it. The entry is named after the path's
    /// final component, as `git worktree add` names a fresh one. The entry's index was deleted with
    /// it, so the index is rebuilt from HEAD, which leaves the working tree untouched.
    pub fn relink_worktree(&self, _lock: &ProjectLock, path: &Path, branch: &str) -> Result<()> {
        let name = path.file_name().ok_or_else(|| {
            crate::error::Error::NotAWorktree(path.to_string_lossy().into_owned())
        })?;
        let admin = self.repo.commondir().join("worktrees").join(name);
        std::fs::create_dir_all(&admin)?;
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", path.join(".git").display()),
        )?;
        std::fs::write(admin.join("commondir"), "../..\n")?;
        std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
        std::fs::write(path.join(".git"), format!("gitdir: {}\n", admin.display()))?;
        git_run(path, ["reset", "-q"])
    }

    /// Enables per-worktree git config via `extensions.worktreeConfig`.
    pub fn enable_config(&self, _lock: &ProjectLock) -> Result<()> {
        git_run(&self.path, ["config", "extensions.worktreeConfig", "true"])
    }

    /// Sets a worktree-scoped git config value.
    pub fn set_config(&self, _lock: &ProjectLock, key: &str, value: &str) -> Result<()> {
        git_run(&self.path, ["config", "--worktree", key, value])
    }

    /// Returns the checked-out branch name, or `None` if HEAD is detached.
    pub fn get_branch(&self) -> Option<String> {
        self.head().ok()?.branch().map(|b| b.name().to_string())
    }

    /// Returns the branch name that HEAD points at, even when the branch has no commits yet.
    /// `None` when HEAD is detached.
    pub fn head_branch_name(&self) -> Option<String> {
        let head = self.repo.find_reference("HEAD").ok()?;
        head.symbolic_target()
            .ok()??
            .strip_prefix("refs/heads/")
            .map(|name| name.to_string())
    }

    /// Whether this repository holds no commits yet, so HEAD names a branch that does not exist.
    pub fn has_no_commits(&self) -> bool {
        self.repo.head().is_err()
    }

    /// Creates a new worktree with a new branch at the given path.
    pub fn create_branch(
        &self,
        _lock: &ProjectLock,
        branch: &str,
        path: &Path,
        git_ref: &impl Revision,
    ) -> Result<()> {
        let sha = git_ref.sha();
        git_run(
            &self.path,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(branch),
                path.as_os_str(),
                OsStr::new(&sha),
            ],
        )
    }

    /// Creates a new worktree at the given path, with a new branch that tracks `remote_ref` (for
    /// example `origin/feature`).
    pub fn create_tracking_branch(
        &self,
        _lock: &ProjectLock,
        branch: &str,
        path: &Path,
        remote_ref: &str,
    ) -> Result<()> {
        git_run(
            &self.path,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--track"),
                OsStr::new("-b"),
                OsStr::new(branch),
                path.as_os_str(),
                OsStr::new(remote_ref),
            ],
        )
    }

    /// Creates a new worktree that checks out an existing branch at the given path.
    pub fn checkout_branch(&self, _lock: &ProjectLock, branch: &str, path: &Path) -> Result<()> {
        git_run(
            &self.path,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                path.as_os_str(),
                OsStr::new(branch),
            ],
        )
    }

    /// Creates a new worktree with a detached HEAD at the given path.
    pub fn checkout_detached(
        &self,
        _lock: &ProjectLock,
        path: &Path,
        git_ref: &impl Revision,
    ) -> Result<()> {
        let sha = git_ref.sha();
        git_run(
            &self.path,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                path.as_os_str(),
                OsStr::new(&sha),
            ],
        )
    }

    /// Lists commits reachable from `tip` but not from `base`, in replay order.
    pub fn list_revisions(&self, base: &impl Revision, tip: &impl Revision) -> Result<Vec<Sha>> {
        let base_sha = base.sha();
        let tip_sha = tip.sha();
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["rev-list", "--reverse"])
            .arg(format!("{base_sha}..{tip_sha}"))
            .output()?;
        if !output.status.success() {
            return Err(crate::error::Error::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(|s| Ok(Sha::new(git2::Oid::from_str(s)?)))
            .collect()
    }

    /// Lists commits reachable from `tip` but not from `base`. Drops any whose patches are already
    /// present on `base` (cherry-pick equivalents).
    /// Lists paths tracked by a revision, in Git's name-only order.
    pub fn tracked_paths(&self, revision: &impl Revision) -> Result<Vec<String>> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["ls-tree", "-r", "--name-only"])
            .arg(revision.sha())
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Lists paths changed by one revision, in Git's name-only order.
    pub fn revision_paths(&self, revision: &impl Revision) -> Result<Vec<String>> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["diff-tree", "--root", "--no-commit-id", "--name-only", "-r"])
            .arg(revision.sha())
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Lists paths changed between two revisions, in Git's name-only order.
    pub fn changed_paths(&self, base: &impl Revision, tip: &impl Revision) -> Result<Vec<String>> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["diff", "--name-only"])
            .args([&base.sha(), &tip.sha()])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Lists revisions that are unique to the tip, in replay order.
    pub fn list_unique_revisions(
        &self,
        base: &impl Revision,
        tip: &impl Revision,
    ) -> Result<Vec<Sha>> {
        let base_sha = base.sha();
        let tip_sha = tip.sha();
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["rev-list", "--reverse", "--cherry-pick", "--right-only"])
            .arg(format!("{base_sha}...{tip_sha}"))
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            Ok(Vec::new())
        } else {
            trimmed
                .split_whitespace()
                .map(|s| Ok(Sha::new(git2::Oid::from_str(s)?)))
                .collect()
        }
    }

    /// Cherry-picks a sequence of commits onto the current branch.
    pub fn cherry_pick_commits(
        &self,
        _lock: &ProjectLock,
        commits: &[impl Revision],
        settings: ClientSettings,
    ) -> Result<()> {
        let shas: Vec<String> = commits.iter().map(|c| c.sha()).collect();
        let args: Vec<&str> = std::iter::once("cherry-pick")
            .chain(shas.iter().map(String::as_str))
            .collect();
        git_run_inherit(&self.path, &args, settings)
    }

    /// Cherry-picks a sequence of commits onto the current branch and captures stdout. Inherits
    /// stdin and stderr, so conflict prompts and diagnostics still reach the human streams.
    pub fn cherry_pick_commits_captured(
        &self,
        _lock: &ProjectLock,
        commits: &[impl Revision],
    ) -> Result<CapturedGit> {
        let shas: Vec<String> = commits.iter().map(|c| c.sha()).collect();
        let args: Vec<&str> = std::iter::once("cherry-pick")
            .chain(shas.iter().map(String::as_str))
            .collect();
        git_run_capture(&self.path, &args)
    }

    /// Stages a sequence of commits without creating commits.
    pub fn stage_commits_captured(
        &self,
        _lock: &ProjectLock,
        commits: &[impl Revision],
    ) -> Result<CapturedGit> {
        let shas: Vec<String> = commits.iter().map(|c| c.sha()).collect();
        git_run_capture(
            &self.path,
            std::iter::once("cherry-pick")
                .chain(std::iter::once("--no-commit"))
                .chain(shas.iter().map(String::as_str)),
        )
    }

    /// Resets the current branch to a commit, while keeping staged changes.
    pub fn soft_reset_captured(
        &self,
        _lock: &ProjectLock,
        revision: &impl Revision,
    ) -> Result<CapturedGit> {
        git_run_capture(&self.path, ["reset", "--soft", &revision.sha()])
    }

    /// Runs `git cherry-pick` with recovery arguments and captures stdout.
    pub fn cherry_pick_captured(&self, _lock: &ProjectLock, args: &[&str]) -> Result<CapturedGit> {
        git_run_capture(
            &self.path,
            std::iter::once("cherry-pick").chain(args.iter().copied()),
        )
    }

    /// Resets a stopped squash merge and captures stdout.
    pub fn reset_merge_captured(&self, _lock: &ProjectLock) -> Result<CapturedGit> {
        git_run_capture(&self.path, ["reset", "--merge"])
    }

    /// Performs a squash merge of the given revision.
    pub fn squash_merge(
        &self,
        _lock: &ProjectLock,
        revision: &impl Revision,
        settings: ClientSettings,
    ) -> Result<()> {
        let sha = revision.sha();
        git_run_inherit(&self.path, ["merge", "--squash", &sha], settings)
    }

    /// Performs a squash merge of the given revision and captures stdout. Inherits stdin and
    /// stderr, so conflict prompts and diagnostics still reach the human streams.
    pub fn squash_merge_captured(
        &self,
        _lock: &ProjectLock,
        revision: &impl Revision,
    ) -> Result<CapturedGit> {
        let sha = revision.sha();
        git_run_capture(&self.path, ["merge", "--squash", &sha])
    }

    /// Creates a commit with the given message.
    pub fn create_commit(
        &self,
        _lock: &ProjectLock,
        message: &str,
        settings: ClientSettings,
    ) -> Result<()> {
        git_run_inherit(&self.path, ["commit", "-m", message], settings)
    }

    /// Creates a commit with the given message and captures stdout. Inherits stdin and stderr, so
    /// conflict prompts and diagnostics still reach the human streams.
    pub fn create_commit_captured(
        &self,
        _lock: &ProjectLock,
        message: &str,
    ) -> Result<CapturedGit> {
        git_run_capture(&self.path, ["commit", "-m", message])
    }

    /// Returns the summary (first line) of a commit.
    pub fn get_commit_summary(&self, revision: &impl Revision) -> Result<String> {
        let oid = git2::Oid::from_str(&revision.sha())?;
        let commit = self.repo.find_commit(oid)?;
        Ok(commit.summary()?.unwrap_or("").to_string())
    }

    /// Runs `git diff --binary` against a base revision and returns the patch.
    pub fn generate_binary_diff(&self, base: &impl Revision) -> Result<BinaryPatch> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["diff", "--binary"])
            .arg(base.sha())
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::error::Error::Command(stderr.trim().to_string()));
        }
        Ok(BinaryPatch::new(output.stdout))
    }

    /// Writes `git archive` of `revision` to `out` as a gzipped tar, with every path below
    /// `prefix`.
    pub fn archive(&self, revision: &impl Revision, prefix: &str, out: &Path) -> Result<()> {
        git_run(
            &self.path,
            [
                OsStr::new("archive"),
                OsStr::new("--format=tar.gz"),
                OsStr::new(&format!("--prefix={prefix}/")),
                OsStr::new("-o"),
                out.as_os_str(),
                OsStr::new(&revision.sha()),
            ],
        )
    }

    /// Whether the tree at `revision` sets the `export-ignore` attribute, which makes
    /// [`Worktree::archive`] leave files out of the archive.
    pub fn exports_ignore(&self, revision: &impl Revision) -> Result<bool> {
        let output = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["grep", "-q", "-e", "export-ignore"])
            .arg(revision.sha())
            .args(["--", ".gitattributes", "*/.gitattributes"])
            .output()?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(crate::error::Error::Command(stderr.trim().to_string()))
            }
        }
    }

    /// Reads `relpath`'s working-tree content as text, or `None` when the file is absent.
    pub fn snapshot_working_file(&self, relpath: &str) -> Result<Option<String>> {
        let bytes = match std::fs::read(self.path.join(relpath)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Applies a binary patch with a three-way merge fallback.
    pub fn apply_three_way_patch(&self, _lock: &ProjectLock, patch: &BinaryPatch) -> Result<()> {
        self.apply_patch(patch, &["--3way"])
    }

    fn apply_patch(&self, patch: &BinaryPatch, extra_args: &[&str]) -> Result<()> {
        let mut child = git_command()
            .arg("-C")
            .arg(&self.path)
            .args(["apply", "--binary"])
            .args(extra_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(patch.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(crate::error::Error::Command(stderr.trim().to_string()))
        }
    }

    /// Lists untracked files in the worktree. Obeys `.gitignore`.
    pub fn list_untracked_files(&self) -> Result<Vec<PathBuf>> {
        let status = self.status()?;
        Ok(status
            .worktree
            .into_iter()
            .filter(|(_, s)| *s == GitFileStatus::Untracked)
            .map(|(p, _)| self.path.join(p))
            .collect())
    }

    fn git_dir(&self) -> PathBuf {
        self.repo.path().to_path_buf()
    }

    /// Returns any in-progress git operation in this worktree.
    pub fn operation_in_progress(&self) -> Option<InProgressOperation> {
        let gd = self.git_dir();
        if gd.join("rebase-merge").is_dir() {
            let steps = read_rebase_steps(&gd.join("rebase-merge"), "msgnum", "end");
            Some(InProgressOperation::Rebase { steps })
        } else if gd.join("rebase-apply").is_dir() {
            let steps = read_rebase_steps(&gd.join("rebase-apply"), "next", "last");
            Some(InProgressOperation::Rebase { steps })
        } else if gd.join("MERGE_HEAD").exists() {
            Some(InProgressOperation::Merge)
        } else if gd.join("CHERRY_PICK_HEAD").exists() {
            Some(InProgressOperation::CherryPick)
        } else if gd.join("REVERT_HEAD").exists() {
            Some(InProgressOperation::Revert)
        } else {
            None
        }
    }

    /// Ahead and behind commit counts of the current branch relative to its upstream tracking
    /// branch. Returns `None` when HEAD is detached or has no upstream.
    pub fn upstream_status(&self) -> Result<Option<UpstreamStatus>> {
        let head = self.repo.head()?;
        if !head.is_branch() {
            return Ok(None);
        }
        let branch = git2::Branch::wrap(head);
        let Ok(upstream) = branch.upstream() else {
            return Ok(None);
        };
        let local_oid = branch.get().peel_to_commit()?.id();
        let upstream_oid = upstream.get().peel_to_commit()?.id();
        let (ahead, behind) = self.repo.graph_ahead_behind(local_oid, upstream_oid)?;
        Ok(Some(UpstreamStatus { ahead, behind }))
    }

    /// Merges unstaged changes and untracked files from this worktree into `dest`, with a three-way
    /// merge fallback.
    pub fn merge_working_tree(&self, lock: &ProjectLock, dest: &Worktree) -> Result<()> {
        let untracked = self.list_untracked_files()?;
        self.validate_untracked_destinations(dest, &untracked)?;

        let head = self.head()?;
        let patch = self.generate_binary_diff(&head)?;
        if !patch.is_empty() {
            dest.apply_three_way_patch(lock, &patch)?;
        }
        self.copy_untracked(dest, &untracked)
    }

    fn validate_untracked_destinations(
        &self,
        dest: &Worktree,
        untracked: &[PathBuf],
    ) -> Result<()> {
        for src_path in untracked {
            let relative = src_path
                .strip_prefix(&self.path)
                .expect("untracked file path must be under worktree path");
            let dst_path = dest.path.join(relative);
            match std::fs::symlink_metadata(&dst_path) {
                Ok(_) => {
                    return Err(crate::error::Error::Command(format!(
                        "untracked destination exists: {}",
                        dst_path.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            for parent in relative
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .filter(|path| !path.as_os_str().is_empty())
            {
                let dst_parent = dest.path.join(parent);
                match std::fs::symlink_metadata(&dst_parent) {
                    Ok(metadata) if metadata.file_type().is_dir() => {}
                    Ok(_) => {
                        return Err(crate::error::Error::Command(format!(
                            "untracked destination parent is not a directory: {}",
                            dst_parent.display()
                        )));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    fn copy_untracked(&self, dest: &Worktree, untracked: &[PathBuf]) -> Result<()> {
        for src_path in untracked {
            let relative = src_path
                .strip_prefix(&self.path)
                .expect("untracked file path must be under worktree path");
            let dst_path = dest.path.join(relative);
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src_path, &dst_path)?;
        }
        Ok(())
    }

    /// Runs `git rebase` with the given arguments. Inherits stdin, stdout, and stderr, so
    /// interactive rebase and conflict output work.
    pub fn rebase(&self, _lock: &ProjectLock, args: &[&str]) -> Result<()> {
        let status = git_command()
            .arg("-C")
            .arg(&self.path)
            .arg("rebase")
            .args(args)
            .status()?;
        rebase_status_result(status)
    }

    /// Runs `git rebase` with the given arguments and captures stdout. A conflicting replay's
    /// `Auto-merging` and `CONFLICT` lines land in the captured stdout, so the caller must forward
    /// them for them to reach the human stream.
    pub fn rebase_captured(&self, _lock: &ProjectLock, args: &[&str]) -> Result<CapturedGit> {
        git_run_capture(
            &self.path,
            std::iter::once("rebase").chain(args.iter().copied()),
        )
    }
}

pub(crate) fn rebase_status_result(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(crate::error::Error::Command(format!(
            "rebase failed (exit {})",
            status.code().unwrap_or(-1)
        )))
    }
}

/// Clones a remote repository into `dest`. Runs `git clone` from `cwd`.
pub fn clone_repo(cwd: &Path, remote: &str, dest: &Path) -> Result<Worktree> {
    git_run(
        cwd,
        [OsStr::new("clone"), OsStr::new(remote), dest.as_os_str()],
    )?;
    Worktree::get(dest)
        .ok_or_else(|| crate::error::Error::NotAWorktree(dest.to_string_lossy().into_owned()))
}

/// Returns the administrative entry name recorded in the `.git` file of the linked worktree at
/// `path`: the final component of its `gitdir:` line. `None` when the file is missing, is a
/// directory, or holds no such line. Git names the entry when the worktree is added and a directory
/// rename leaves it behind, so the entry name and the directory name can diverge.
pub fn linked_entry_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path.join(".git")).ok()?;
    let gitdir = text.strip_prefix("gitdir:")?.trim();
    Path::new(gitdir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Moves a worktree to a new path and returns the relocated worktree.
///
/// Re-opens the worktree after the move, because `git2::Repository` caches the original path
/// internally.
pub fn rename_worktree(worktree: Worktree, new_path: &Path) -> Result<Worktree> {
    std::fs::rename(worktree.path(), new_path)?;
    Worktree::get(new_path)
        .ok_or_else(|| crate::error::Error::NotAWorktree(new_path.to_string_lossy().into_owned()))
}

/// The stdout and process status of one captured git command.
pub struct CapturedGit {
    /// The normal output of the command.
    pub stdout: String,
    /// The process status of the command.
    pub status: std::process::ExitStatus,
}

/// Runs one git subcommand in `cwd` and captures its stdout. It inherits stdin and stderr.
pub fn git_capture(
    cwd: &Path,
    subcommand: &str,
    rev: Option<&str>,
    args: &[String],
) -> Result<CapturedGit> {
    let mut argv: Vec<&str> = vec![subcommand];
    argv.extend(rev);
    argv.extend(args.iter().map(String::as_str));
    git_run_capture(cwd, argv)
}

/// Starts Git without repository paths inherited from a hook or another worktree.
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
        "GIT_QUARANTINE_PATH",
        "GIT_PREFIX",
        "GIT_INTERNAL_SUPER_PREFIX",
        "GIT_IMPLICIT_WORK_TREE",
    ] {
        command.env_remove(key);
    }
    command
}

/// Runs one git subcommand in `cwd` and captures its stdout. Inherits stdin and stderr. Does not
/// treat a nonzero exit as an error, so the caller can forward the captured lines before deciding
/// how to report the failure.
fn git_run_capture<I, S>(cwd: &Path, args: I) -> Result<CapturedGit>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_command()
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    Ok(CapturedGit {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        status: output.status,
    })
}

fn git_run<I, S>(cwd: &Path, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_command().arg("-C").arg(cwd).args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(crate::error::Error::Command(stderr.trim().to_string()))
    }
}

fn git_run_inherit<I, S>(cwd: &Path, args: I, settings: ClientSettings) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let stdout = if settings.quiet() {
        Stdio::null()
    } else {
        Stdio::inherit()
    };
    let status = git_command()
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(stdout)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        Err(crate::error::Error::Command(format!(
            "git exited with status {code}"
        )))
    }
}

fn read_rebase_steps(dir: &Path, done_file: &str, total_file: &str) -> Option<RebaseSteps> {
    let done = read_count(&dir.join(done_file))?;
    let total = read_count(&dir.join(total_file))?;
    Some(RebaseSteps { done, total })
}

fn read_count(path: &Path) -> Option<usize> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn worktree_file_status(s: git2::Status) -> Option<GitFileStatus> {
    if s.is_wt_new() {
        Some(GitFileStatus::Untracked)
    } else if s.is_wt_modified() || s.is_wt_typechange() {
        Some(GitFileStatus::Modified)
    } else if s.is_wt_deleted() {
        Some(GitFileStatus::Deleted)
    } else {
        None
    }
}

fn index_file_status(s: git2::Status) -> Option<GitFileStatus> {
    if s.is_index_new() {
        Some(GitFileStatus::Added)
    } else if s.is_index_modified() || s.is_index_typechange() {
        Some(GitFileStatus::Modified)
    } else if s.is_index_deleted() {
        Some(GitFileStatus::Deleted)
    } else if s.is_index_renamed() {
        Some(GitFileStatus::Renamed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{commit, git, rev};

    fn temp_worktree(initial: &str) -> (tempfile::TempDir, Worktree) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("file.txt"), initial).unwrap();
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "initial"]);
        let wt = Worktree::get(dir.path()).unwrap();
        (dir, wt)
    }

    fn merge_worktrees() -> (
        tempfile::TempDir,
        Worktree,
        Worktree,
        crate::project::Project,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source");
        let destination_path = dir.path().join("destination");
        for path in [&source_path, &destination_path] {
            std::fs::create_dir_all(path).unwrap();
            git(path, &["init", "-q"]);
            std::fs::write(path.join("file.txt"), "initial\n").unwrap();
            git(path, &["add", "file.txt"]);
            git(path, &["commit", "-q", "-m", "initial"]);
        }
        let project = crate::project::Project::with_dirs(
            "test".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("config"),
            dir.path().join("state"),
        );
        (
            dir,
            Worktree::get(&source_path).unwrap(),
            Worktree::get(&destination_path).unwrap(),
            project,
        )
    }

    #[test]
    fn head_branch_name_resolves_an_unborn_branch() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["symbolic-ref", "HEAD", "refs/heads/stem"]);
        let wt = Worktree::get(dir.path()).unwrap();
        assert_eq!(wt.head_branch_name().as_deref(), Some("stem"));
        assert_eq!(wt.get_branch(), None);
    }

    #[test]
    fn head_branch_name_matches_get_branch_and_is_none_when_detached() {
        let (dir, wt) = temp_worktree("a\n");
        assert_eq!(wt.head_branch_name(), wt.get_branch());

        git(dir.path(), &["checkout", "-q", "--detach"]);
        let wt = Worktree::get(dir.path()).unwrap();
        assert_eq!(wt.head_branch_name(), None);
    }

    #[test]
    fn is_ancestor_holds_on_advance_and_fails_after_rewrite() {
        let (dir, wt) = temp_worktree("a\n");
        let base = rev(dir.path(), "HEAD");

        commit(dir.path(), "a\nb\n", "advance");
        let advanced = rev(dir.path(), "HEAD");
        assert!(wt.is_ancestor(&base, &advanced).unwrap());

        git(dir.path(), &["commit", "-q", "--amend", "-m", "rewritten"]);
        let rewritten = rev(dir.path(), "HEAD");
        assert!(!wt.is_ancestor(&advanced, &rewritten).unwrap());
    }

    #[test]
    fn merge_base_returns_the_fork_commit() {
        let (dir, wt) = temp_worktree("a\n");
        let fork = rev(dir.path(), "HEAD");

        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        commit(dir.path(), "a\nfeature\n", "feature");
        let feature = rev(dir.path(), "HEAD");

        git(dir.path(), &["checkout", "-q", fork.sha().as_str()]);
        commit(dir.path(), "a\nmain\n", "main");
        let main = rev(dir.path(), "HEAD");

        assert_eq!(wt.merge_base(&feature, &main).unwrap(), fork);
    }

    #[test]
    fn merge_base_errors_on_disjoint_roots() {
        let (dir, wt) = temp_worktree("a\n");
        let root = rev(dir.path(), "HEAD");

        git(dir.path(), &["checkout", "-q", "--orphan", "island"]);
        commit(dir.path(), "island\n", "island root");
        let island = rev(dir.path(), "HEAD");

        match wt.merge_base(&root, &island) {
            Err(crate::error::Error::NoCommonAncestor { .. }) => {}
            other => panic!("expected NoCommonAncestor, got {other:?}"),
        }
    }

    #[test]
    fn commits_only_in_lists_dropped_commits_oldest_first() {
        let (dir, wt) = temp_worktree("0\n");
        let fork = rev(dir.path(), "HEAD");

        commit(dir.path(), "0\n1\n", "one");
        let one = rev(dir.path(), "HEAD");
        commit(dir.path(), "0\n1\n2\n", "two");
        let anchor = rev(dir.path(), "HEAD");

        git(dir.path(), &["reset", "-q", "--hard", fork.sha().as_str()]);
        commit(dir.path(), "0\nrewritten\n", "rewritten");
        let rewritten_tip = rev(dir.path(), "HEAD");

        let dropped = wt.commits_only_in(&anchor, &rewritten_tip).unwrap();
        assert_eq!(dropped, vec![one, anchor]);

        assert!(wt.commits_only_in(&fork, &anchor).unwrap().is_empty());
    }

    /// Commits `content` at `relpath`, creating the parent directories.
    fn commit_file(dir: &Path, relpath: &str, content: &str, message: &str) {
        let path = dir.join(relpath);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        git(dir, &["add", relpath]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    #[test]
    fn exports_ignore_is_false_without_a_gitattributes() {
        // `git grep` exits 1 here, not 128. A snapshot of such a tree must warn about nothing
        // rather than abort.
        let (dir, wt) = temp_worktree("a\n");

        assert!(!wt.exports_ignore(&rev(dir.path(), "HEAD")).unwrap());
    }

    #[test]
    fn exports_ignore_finds_a_root_gitattributes() {
        let (dir, wt) = temp_worktree("a\n");
        commit_file(
            dir.path(),
            ".gitattributes",
            "docs export-ignore\n",
            "ignore docs",
        );

        assert!(wt.exports_ignore(&rev(dir.path(), "HEAD")).unwrap());
    }

    #[test]
    fn exports_ignore_finds_a_nested_gitattributes() {
        // The pathspec `*/.gitattributes` relies on `*` crossing `/`, so a file more than one
        // directory down must match.
        let (dir, wt) = temp_worktree("a\n");
        commit_file(
            dir.path(),
            "a/b/.gitattributes",
            "x export-ignore\n",
            "ignore x",
        );

        assert!(wt.exports_ignore(&rev(dir.path(), "HEAD")).unwrap());
    }

    #[test]
    fn git_commands_ignore_inherited_repository_paths() {
        if let Some(path) = std::env::var_os("SUBSPACE_GIT_ENV_TEST_PATH") {
            let captured = git_capture(
                Path::new(&path),
                "rev-parse",
                None,
                &["--show-toplevel".to_string()],
            )
            .unwrap();
            assert!(captured.status.success());
            assert_eq!(
                Path::new(captured.stdout.trim()).canonicalize().unwrap(),
                Path::new(&path).canonicalize().unwrap()
            );
            git_run(Path::new(&path), ["checkout", "-b", "env-test"]).unwrap();
            assert!(
                Worktree::get(Path::new(&path))
                    .unwrap()
                    .has_branch("env-test")
            );
            let other = std::env::var_os("GIT_WORK_TREE").unwrap();
            assert!(
                !Worktree::get(Path::new(&other))
                    .unwrap()
                    .has_branch("env-test")
            );
            return;
        }
        let (dir, _wt) = temp_worktree("a\n");
        let (other, _other_wt) = temp_worktree("b\n");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worktree::tests::git_commands_ignore_inherited_repository_paths",
            ])
            .env("SUBSPACE_GIT_ENV_TEST_PATH", dir.path())
            .env("GIT_DIR", other.path().join(".git"))
            .env("GIT_WORK_TREE", other.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn git_capture_places_the_revision_before_the_arguments() {
        let (dir, _wt) = temp_worktree("a\n");
        commit(dir.path(), "a\nb\n", "advance");
        let head = rev(dir.path(), "HEAD");

        let captured = git_capture(
            dir.path(),
            "log",
            Some(head.sha().as_str()),
            &["-1".to_string(), "--format=%s".to_string()],
        )
        .unwrap();

        assert!(captured.status.success());
        assert_eq!(captured.stdout, "advance\n");
    }

    #[test]
    fn git_capture_omits_the_revision_when_there_is_none() {
        let (dir, _wt) = temp_worktree("a\n");

        let captured = git_capture(
            dir.path(),
            "rev-parse",
            None,
            &["--is-inside-work-tree".to_string()],
        )
        .unwrap();

        assert!(captured.status.success());
        assert_eq!(captured.stdout.trim(), "true");
    }

    #[test]
    fn git_capture_returns_a_failed_status_without_an_error() {
        let (dir, _wt) = temp_worktree("a\n");

        // `git config --get` of an absent key exits 1 and writes nothing.
        let captured = git_capture(
            dir.path(),
            "config",
            None,
            &["--get".to_string(), "no.such.key".to_string()],
        )
        .unwrap();

        assert!(!captured.status.success());
        assert_eq!(captured.stdout, "");
    }

    #[test]
    fn exports_ignore_errors_on_a_revision_the_repository_lacks() {
        let (_dir, wt) = temp_worktree("a\n");
        let absent =
            Sha::new(git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap());

        assert!(matches!(
            wt.exports_ignore(&absent),
            Err(crate::error::Error::Command(_))
        ));
    }

    #[test]
    fn merge_working_tree_rejects_existing_untracked_files_before_applying_a_patch() {
        let (_dir, source, destination, project) = merge_worktrees();
        std::fs::write(source.path().join("file.txt"), "source change\n").unwrap();
        std::fs::write(source.path().join("copy.txt"), "source copy\n").unwrap();
        std::fs::write(destination.path().join("copy.txt"), "destination copy\n").unwrap();
        let lock = project.lock("test").unwrap();

        assert!(source.merge_working_tree(&lock, &destination).is_err());

        assert_eq!(
            std::fs::read_to_string(destination.path().join("file.txt")).unwrap(),
            "initial\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join("copy.txt")).unwrap(),
            "destination copy\n"
        );
        assert!(destination.status().unwrap().index.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn merge_working_tree_rejects_destination_symlinks_before_applying_a_patch() {
        let (_dir, source, destination, project) = merge_worktrees();
        std::fs::write(source.path().join("file.txt"), "source change\n").unwrap();
        std::fs::write(source.path().join("copy.txt"), "source copy\n").unwrap();
        std::fs::write(destination.path().join("protected.txt"), "protected\n").unwrap();
        std::os::unix::fs::symlink("protected.txt", destination.path().join("copy.txt")).unwrap();
        let lock = project.lock("test").unwrap();

        assert!(source.merge_working_tree(&lock, &destination).is_err());

        assert!(
            std::fs::symlink_metadata(destination.path().join("copy.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join("protected.txt")).unwrap(),
            "protected\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join("file.txt")).unwrap(),
            "initial\n"
        );
        assert!(destination.status().unwrap().index.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn merge_working_tree_rejects_nested_symlink_escape_before_applying_a_patch() {
        let (dir, source, destination, project) = merge_worktrees();
        std::fs::write(source.path().join("file.txt"), "source change\n").unwrap();
        std::fs::create_dir_all(source.path().join("nested/child")).unwrap();
        std::fs::write(source.path().join("nested/child/copy.txt"), "source copy\n").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(destination.path().join("nested")).unwrap();
        std::os::unix::fs::symlink(&outside, destination.path().join("nested/child")).unwrap();
        let lock = project.lock("test").unwrap();

        assert!(source.merge_working_tree(&lock, &destination).is_err());
        assert!(!outside.join("copy.txt").exists());
        assert_eq!(
            std::fs::read_to_string(destination.path().join("file.txt")).unwrap(),
            "initial\n"
        );
        assert!(destination.status().unwrap().index.is_empty());
    }

    #[test]
    fn merge_working_tree_rejects_non_directory_destination_parents() {
        let (_dir, source, destination, project) = merge_worktrees();
        std::fs::write(source.path().join("file.txt"), "source change\n").unwrap();
        std::fs::create_dir_all(source.path().join("nested")).unwrap();
        std::fs::write(source.path().join("nested/copy.txt"), "source copy\n").unwrap();
        std::fs::write(destination.path().join("nested"), "destination file\n").unwrap();
        let lock = project.lock("test").unwrap();

        assert!(source.merge_working_tree(&lock, &destination).is_err());

        assert_eq!(
            std::fs::read_to_string(destination.path().join("nested")).unwrap(),
            "destination file\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join("file.txt")).unwrap(),
            "initial\n"
        );
        assert!(destination.status().unwrap().index.is_empty());
    }

    #[test]
    fn status_reports_the_new_path_of_a_staged_rename() {
        let (dir, worktree) = temp_worktree("initial\n");
        git(dir.path(), &["mv", "file.txt", "renamed.txt"]);
        std::fs::write(dir.path().join("renamed.txt"), "edited after rename\n").unwrap();

        let status = worktree.status().unwrap();
        assert!(
            status
                .index
                .contains(&(PathBuf::from("renamed.txt"), GitFileStatus::Renamed))
        );
        assert!(
            status
                .worktree
                .contains(&(PathBuf::from("renamed.txt"), GitFileStatus::Modified))
        );
    }

    #[test]
    fn status_reads_real_unmerged_paths_and_unusual_names() {
        let (dir, worktree) = temp_worktree("initial\n");
        let unusual = "space and\t tab\nline.txt";
        std::fs::write(dir.path().join(unusual), "untracked\n").unwrap();
        git(dir.path(), &["branch", "other"]);
        git(dir.path(), &["checkout", "-q", "other"]);
        commit(dir.path(), "other\n", "other change");
        git(dir.path(), &["checkout", "-q", "-"]);
        commit(dir.path(), "current\n", "current change");
        let output = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["merge", "--no-edit", "other"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let status = worktree.status().unwrap();
        assert!(
            status
                .worktree
                .contains(&(PathBuf::from("file.txt"), GitFileStatus::Unmerged))
        );
        assert!(
            status
                .worktree
                .contains(&(PathBuf::from(unusual), GitFileStatus::Untracked))
        );
    }

    #[test]
    fn merge_working_tree_copies_untracked_files_to_new_destinations() {
        let (_dir, source, destination, project) = merge_worktrees();
        std::fs::write(source.path().join("copy.txt"), "source copy\n").unwrap();
        std::fs::create_dir_all(source.path().join("nested")).unwrap();
        std::fs::write(source.path().join("nested/copy.txt"), "nested copy\n").unwrap();
        let lock = project.lock("test").unwrap();

        source.merge_working_tree(&lock, &destination).unwrap();

        assert_eq!(
            std::fs::read_to_string(destination.path().join("copy.txt")).unwrap(),
            "source copy\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join("nested/copy.txt")).unwrap(),
            "nested copy\n"
        );
        assert!(destination.status().unwrap().index.is_empty());
    }
}
