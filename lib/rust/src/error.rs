use std::path::PathBuf;

use crate::revision::Sha;

/// Alias for `std::result::Result` with the library [`Error`] type.
pub type Result<T> = std::result::Result<T, Error>;

/// Library error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `HOME` environment variable is not set, so the registry path cannot resolve.
    #[error("HOME is not set")]
    HomeNotSet,

    /// The named project does not exist.
    #[error("project not found: {0}")]
    ProjectNotFound(String),

    /// The named project already exists.
    #[error("project already exists: {0}")]
    ProjectExists(String),

    /// The named workspace does not exist.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    /// The named workspace already exists.
    #[error("workspace already exists: {0}")]
    WorkspaceExists(String),

    /// The path is not a git worktree.
    #[error("not a git worktree: {0}")]
    NotAWorktree(String),

    /// An unrecognized sigil was provided.
    #[error("unknown sigil: {0}")]
    UnknownSigil(String),

    /// A cycle was detected in a parent chain.
    #[error("cycle detected in parent chain at: {0}")]
    CycleDetected(String),

    /// A workspace names a parent that does not exist on disk.
    #[error("workspace '{workspace}' references a missing parent: {parent}")]
    DanglingParent {
        /// The workspace carrying the dangling parent reference.
        workspace: String,
        /// The parent that the metadata names, which nothing found.
        parent: String,
    },

    /// A parent workspace rewrote history at or below a child's branch point.
    #[error(
        "parent of '{workspace}' has rewritten history below the branch point{}\nrun `subspace rebase` in '{workspace}' to repair",
        format_dropped(.dropped)
    )]
    ParentRewritten {
        /// The child workspace whose chain carries the rewritten edge.
        workspace: String,
        /// Commits the parent dropped but the child's anchor still reaches.
        dropped: Vec<Sha>,
    },

    /// The stored rebase anchor is no longer in the workspace history.
    #[error(
        "stored rebase anchor {anchor} is not an ancestor of HEAD in '{workspace}'\nchoose a replay boundary with `git rebase --onto <base-branch> <last-excluded-commit>`\nthen run `subspace rebase <base-workspace>` to update the anchor"
    )]
    StaleRebaseAnchor {
        /// The workspace whose history no longer contains the stored anchor.
        workspace: String,
        /// The stored anchor outside the workspace history.
        anchor: Sha,
    },

    /// Two revisions share no common ancestor.
    #[error("no common ancestor between {a} and {b}")]
    NoCommonAncestor {
        /// The first revision.
        a: String,
        /// The second revision.
        b: String,
    },

    /// The current directory is not inside a project or workspace.
    #[error("not inside a project or workspace")]
    NoScope,

    /// An invalid workspace or project name was given. The payload names which kind it is, so it
    /// stands alone as the message.
    #[error("{0}")]
    InvalidName(String),

    /// No default workspace is set for the project.
    #[error("no default workspace set for {0}")]
    NoDefaultWorkspace(String),

    /// Attempted to remove the default workspace.
    #[error("cannot remove the default workspace")]
    IsDefaultWorkspace,

    /// A target resolved to a different project than expected.
    #[error("target must be within project '{expected}', got '{actual}'")]
    CrossProjectTarget {
        /// The project that the target was expected to be within.
        expected: String,
        /// The target string that resolved to a different project.
        actual: String,
    },

    /// The command was issued from a workspace outside the project it is scoped to.
    #[error(
        "command is scoped to project '{scope}' but was issued from a workspace of project '{origin}'"
    )]
    CrossProjectOrigin {
        /// The project the command is scoped to.
        scope: String,
        /// The project holding the workspace the command was issued from.
        origin: String,
    },

    /// No usable worktree exists in the project.
    #[error("no valid worktree found in project: {0}")]
    NoWorktree(String),

    /// The metadata records no branch for the workspace.
    #[error("no branch recorded for workspace: {0}")]
    NoRecordedBranch(String),

    /// The project mutation lock is already held by another process.
    #[error("project locked by: {0}")]
    ProjectLocked(String),

    /// A bare ref matched a remote-tracking branch in more than one remote.
    #[error(
        "'{0}' matched remote-tracking branches in more than one remote\nqualify it as <remote>/{0}"
    )]
    AmbiguousRemoteBranch(String),

    /// An external command failed.
    #[error("command failed: {0}")]
    Command(String),

    /// A configured command carries a placeholder that cannot be expanded.
    #[error("{0}")]
    Placeholder(String),

    /// A configured hook command failed to expand, failed to spawn, or exited non-zero.
    #[error("{event} hook: {message}")]
    HookFailed {
        /// The event whose command failed.
        event: String,
        /// What went wrong.
        message: String,
    },

    /// More than one `_post` hook ran for one mutation, and at least one failed. Every hook still
    /// ran, because the change had already landed by the time any of them started, so this lists
    /// each failure instead of naming only the first.
    #[error("post hooks failed, and the change stays{}", format_hook_failures(.events))]
    HooksFailed {
        /// The message of each hook that failed, in the order it ran.
        events: Vec<String>,
    },

    /// `~/.config/subspace/config.json` failed to parse or validate. It carries the path because
    /// this document is read on every command, so an unattributed error follows the user
    /// around with nothing to act on.
    #[error("{}: {message}", .path.display())]
    InvalidSettings {
        /// The document that failed.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },

    /// One setting is not valid. Raised while a settings document deserializes, so it carries no
    /// framing of its own: serde takes the text and [`Error::InvalidSettings`] adds the path.
    #[error("{0}")]
    Setting(String),

    /// A commit SHA string was not valid hex.
    #[error("invalid SHA {0:?}")]
    InvalidSha(String),

    /// A snapshot of that workspace and commit is already on disk.
    #[error("snapshot already exists: {0}")]
    SnapshotExists(String),

    /// A file name is not in the `<workspace>-<sha>.tar.gz` form.
    #[error("not a snapshot file name: {0}\nexpected <workspace>-<sha>.tar.gz")]
    InvalidSnapshotName(String),

    /// The commit a snapshot names does not resolve in the project's repository, so there is
    /// nothing to restore from. The cause tells absence apart from an ambiguous abbreviation, which
    /// a longer sha corrects.
    #[error("snapshot {file} names commit {sha}, which does not resolve in this project: {cause}")]
    SnapshotCommitMissing {
        /// The snapshot file.
        file: String,
        /// The commit its name carries.
        sha: String,
        /// What git reported when the sha did not resolve.
        cause: String,
    },

    /// A `git2` library error.
    #[error(transparent)]
    Git(#[from] git2::Error),

    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A JSON serialization/deserialization error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The on-disk project schema version does not match the expected version.
    #[error(
        "project schema version mismatch: found {found}, expected {expected}\n{}",
        format_version_guidance(.found, .expected)
    )]
    VersionMismatch {
        /// The version found on disk.
        found: u64,
        /// The version this binary expects.
        expected: u64,
    },
}

/// Formats the guidance line of [`Error::VersionMismatch`].
fn format_version_guidance(found: &u64, expected: &u64) -> String {
    if found < expected {
        "this project schema is no longer supported; recreate the project".to_string()
    } else {
        "upgrade the subspace binary".to_string()
    }
}

/// Formats the dropped-commit lines of [`Error::ParentRewritten`].
fn format_dropped(dropped: &[Sha]) -> String {
    dropped
        .iter()
        .map(|sha| format!("\n  dropped {sha}"))
        .collect()
}

/// Formats the failure lines of [`Error::HooksFailed`].
fn format_hook_failures(events: &[String]) -> String {
    events.iter().map(|event| format!("\n  {event}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_project_schema_requires_recreation() {
        let message = Error::VersionMismatch {
            found: 10,
            expected: 11,
        }
        .to_string();

        assert!(message.contains("recreate the project"));
        assert!(!message.contains("subspace-migrate"));
    }
}
