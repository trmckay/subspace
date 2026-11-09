use std::{collections::HashMap, path::PathBuf};

/// Status of a single file in the index or worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitFileStatus {
    /// A newly added file.
    Added,
    /// A modified file.
    Modified,
    /// A deleted file.
    Deleted,
    /// An untracked file.
    Untracked,
    /// A renamed file.
    Renamed,
    /// A file with unresolved merge conflicts.
    Unmerged,
}

/// Combined index and worktree file statuses.
#[derive(Debug, Default)]
pub struct GitStatus {
    /// Staged file statuses.
    pub index: Vec<(PathBuf, GitFileStatus)>,
    /// Unstaged file statuses.
    pub worktree: Vec<(PathBuf, GitFileStatus)>,
}

impl GitStatus {
    fn count(entries: &[(PathBuf, GitFileStatus)]) -> HashMap<GitFileStatus, usize> {
        entries.iter().fold(HashMap::new(), |mut counts, (_, s)| {
            *counts.entry(*s).or_insert(0) += 1;
            counts
        })
    }

    fn index_counts(&self) -> HashMap<GitFileStatus, usize> {
        GitStatus::count(&self.index)
    }

    fn worktree_counts(&self) -> HashMap<GitFileStatus, usize> {
        GitStatus::count(&self.worktree)
    }

    /// Returns combined index + worktree status counts.
    pub fn all_counts(&self) -> HashMap<GitFileStatus, usize> {
        let mut counts = self.index_counts();
        for (s, n) in self.worktree_counts() {
            *counts.entry(s).or_insert(0) += n;
        }
        counts
    }
}
