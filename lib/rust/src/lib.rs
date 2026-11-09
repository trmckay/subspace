#![warn(missing_docs)]

//! Workspace and project management library.

/// Per-invocation client settings.
pub mod client;
/// Error types.
pub mod error;
/// Filesystem utilities.
pub mod fs;
/// Git status types.
pub mod git;
/// GitHub pull request lookup.
pub mod github;
/// Commands the user configures subspace to run after a mutation.
pub mod hook;
/// Project metadata.
pub mod metadata;
mod name;
/// Global path resolution through the XDG base directories.
pub mod paths;
/// Project discovery and structure.
pub mod project;
/// Global project registry (`~/.local/state/subspace/projects.json`).
pub mod registry;
/// Git revision types.
pub mod revision;
/// Scope resolution.
pub mod scope;
/// This user's global settings (`~/.config/subspace/config.json`).
pub mod settings;
/// Workspace snapshots: a tar archive of one commit.
pub mod snapshot;
/// Shared git helpers for tests.
#[cfg(test)]
pub(crate) mod test_support;
/// Tree trait and traversal.
pub mod tree;
/// Workspace operations.
pub mod workspace;
/// Git worktree operations.
pub mod worktree;
