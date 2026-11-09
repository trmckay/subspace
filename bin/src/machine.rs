//! The machine documents that `--json` writes, and the helpers that build them.
//!
//! Each command has its own success shape. This module holds every one of those shapes, so the
//! contract sits in one place and a reader can check it without reading the commands. The channel
//! that writes a shape lives in `output`, and the commands that fill one live in `main`.

use std::{collections::HashMap, path::Path};

use anyhow::Result;
use serde::Serialize;
use subspace::{
    git::GitFileStatus,
    metadata::ProjectMeta,
    project::Project,
    revision::{Revision, Sha},
    workspace::{RelinkPlan, RepairPlan, Unrepairable, WorkspaceInfo, WorkspaceTree},
    worktree::InProgressOperation,
};

use crate::output;

/// The machine result of a command that names one workspace: `root` and `parent`.
#[derive(Serialize)]
pub struct NameJson<'a> {
    pub name: &'a str,
}

/// The machine result of `current`: the project and the bare workspace name.
#[derive(Serialize)]
pub struct CurrentJson<'a> {
    pub project: &'a str,
    pub name: &'a str,
}

/// The machine result of `path`: the directory of the scope, and which kind of scope it is.
#[derive(Serialize)]
pub struct PathJson<'a> {
    pub path: String,
    pub scope: &'a str,
}

/// The machine result of a project setting read or write: `default` and `branch-prefix`. The value
/// is the setting after the command, and null when the project has none.
#[derive(Serialize)]
pub struct SettingJson<'a> {
    pub project: &'a str,
    pub setting: &'a str,
    pub value: Option<&'a str>,
}

/// The machine result of the branch recorded for one workspace.
#[derive(Serialize)]
pub struct WorkspaceSettingJson<'a> {
    pub project: &'a str,
    pub workspace: &'a str,
    pub setting: &'a str,
    pub value: Option<&'a str>,
}

/// The machine result of a pass-through git command: `diff` and `log`. The range is the resolved
/// revision range, or null when the command names none. The arguments are the ones that git
/// receives after the range. The output is the normal git stdout.
#[derive(Serialize)]
pub struct GitPassthroughJson<'a> {
    pub command: &'a str,
    pub range: Option<&'a str>,
    pub args: &'a [String],
    pub stdout: &'a str,
}

/// The machine result of `project current`: the project and its directory.
#[derive(Serialize)]
pub struct ProjectJson<'a> {
    pub project: &'a str,
    pub path: String,
}

/// The machine result of `project remove`: the project, its directory, and every resource that the
/// command deleted. The list is empty when the command only deregistered the project.
#[derive(Serialize)]
pub struct ProjectRemovedJson<'a> {
    pub project: &'a str,
    pub path: String,
    pub removed: Vec<String>,
}

/// The machine result of `project rename`: the new name, the old name, the directory of the project
/// after the rename, and the directory before the rename.
#[derive(Serialize)]
pub struct ProjectRenameJson<'a> {
    pub project: &'a str,
    pub previous: &'a str,
    pub path: String,
    pub previous_path: String,
}

/// The machine result of `project move`: the project, its new directory, and its old directory.
#[derive(Serialize)]
pub struct ProjectMoveJson<'a> {
    pub project: &'a str,
    pub path: String,
    pub previous_path: String,
}

/// The machine result of a project creation: `project clone` and `project init`.
/// The workspace is the root workspace that the new project starts with.
#[derive(Serialize)]
pub struct ProjectCreatedJson<'a> {
    pub project: &'a str,
    pub path: String,
    pub workspace: &'a str,
}

/// One parent change that a workspace command makes. The parent is null when the workspace becomes
/// a root workspace.
#[derive(Serialize)]
pub struct ReparentJson<'a> {
    pub workspace: &'a str,
    pub parent: Option<&'a str>,
}

/// The machine result of `add` and `branch`. The branch is null for a detached checkout. The parent
/// is null for a root workspace. The reparented list holds the child that `--parent-of` moves onto
/// the new workspace, and is empty otherwise.
#[derive(Serialize)]
pub struct WorkspaceCreatedJson<'a> {
    pub project: &'a str,
    pub workspace: &'a str,
    pub path: String,
    pub branch: Option<&'a str>,
    pub parent: Option<&'a str>,
    pub reparented: Vec<ReparentJson<'a>>,
}

/// One workspace that `remove` deleted. The branch is the checked-out branch alone, and is null
/// for a detached worktree. The deleted_branch is the branch the command deleted, which can differ
/// from branch for a detached worktree, or null if the command deleted no branch.
#[derive(Serialize)]
pub struct RemovedWorkspaceJson {
    pub workspace: String,
    pub path: String,
    pub branch: Option<String>,
    pub deleted_branch: Option<String>,
}

/// The machine result of `remove`. The removed list holds one entry per deleted workspace, in
/// removal order. The reparented list holds each child that the removal moved onto a live ancestor.
/// The killed list names every session that the command killed.
#[derive(Serialize)]
pub struct WorkspaceRemovedJson<'a> {
    pub project: &'a str,
    pub removed: Vec<RemovedWorkspaceJson>,
    pub reparented: Vec<ReparentJson<'a>>,
}

/// The machine result of `rename`: the project, both names, both directories, and every session
/// that the command killed.
#[derive(Serialize)]
pub struct WorkspaceRenameJson<'a> {
    pub project: &'a str,
    pub workspace: &'a str,
    pub previous: &'a str,
    pub path: String,
    pub previous_path: String,
}

/// One worktree link that a repair rebuilt, or will rebuild in a dry run.
#[derive(Serialize)]
pub struct RelinkJson<'a> {
    pub workspace: &'a str,
    pub branch: &'a str,
}

/// One broken worktree link that a repair cannot rebuild, with the reason as one stable code and
/// one message.
#[derive(Serialize)]
pub struct UnrepairableJson<'a> {
    pub workspace: &'a str,
    pub reason: &'static str,
    pub message: String,
    pub branch: Option<&'a str>,
    pub holder: Option<&'a str>,
}

/// The machine result of both repair modes. `dry_run` says which mode ran, and `links_checked` says
/// whether the worktree-link half ran at all. A dry run reports what it would change, and a real
/// run reports what it changed.
#[derive(Serialize)]
pub struct RepairJson<'a> {
    pub project: &'a str,
    pub dry_run: bool,
    pub links_checked: bool,
    pub relinked: Vec<RelinkJson<'a>>,
    pub unrepairable: Vec<UnrepairableJson<'a>>,
    pub pruned: Vec<&'a str>,
    pub reparented: Vec<ReparentJson<'a>>,
    pub cleared_default: Option<&'a str>,
}

/// One parent change with owned names. A result that outlives the metadata it read uses this form
/// instead of [`ReparentJson`].
#[derive(Serialize)]
pub struct OwnedReparentJson {
    pub workspace: String,
    pub parent: Option<String>,
}

/// The removal of a merge source. The branch is the checked-out branch of the source, and is null
/// for a detached worktree. The deleted_branch is the branch that the removal deleted, and is null
/// when it deleted none. The reparented list holds each child that moved onto the parent of the
/// source.
#[derive(Serialize)]
pub struct MergeRemovedJson {
    pub workspace: String,
    pub path: String,
    pub branch: Option<String>,
    pub deleted_branch: Option<String>,
    pub reparented: Vec<OwnedReparentJson>,
}

/// The machine result of `merge`. The commits are the revisions that the merge replayed, in replay
/// order, and the list is empty when the source carries none. The removed field holds the removal
/// of the source workspace, and is null when the merge kept it.
#[derive(Serialize)]
pub struct MergeJson<'a> {
    pub project: &'a str,
    pub workspace: &'a str,
    pub source: &'a str,
    pub squash: bool,
    pub unstaged: bool,
    pub commits: Vec<Sha>,
    pub removed: Option<MergeRemovedJson>,
}

/// The integration plan returned by a merge or rebase preview.
#[derive(Serialize)]
pub struct IntegrationPlan {
    pub effects: serde_json::Value,
    pub warnings: Vec<String>,
    pub confirmation: String,
    pub applied_commits: Vec<String>,
    pub dropped_commits: Vec<String>,
    pub anchors: Vec<String>,
    pub changed_paths: Vec<String>,
    pub overlap_paths: Vec<String>,
    pub requested_unstaged_paths: Vec<String>,
    pub untracked_collisions: Vec<String>,
    pub stopped: bool,
}

/// The machine result of an integration preview.
#[derive(Serialize)]
pub struct IntegrationPreview {
    pub plan: IntegrationPlan,
}

/// The machine result of `integration continue` and `integration abort`.
#[derive(Serialize)]
pub struct IntegrationJson<'a> {
    pub project: &'a str,
    pub workspace: &'a str,
    pub operation: &'a str,
    pub outcome: &'a str,
    pub head: String,
}

/// The machine result of `rebase`.
#[derive(Serialize)]
pub struct RebaseJson<'a> {
    pub project: &'a str,
    pub workspace: &'a str,
    pub base: &'a str,
    pub base_head: String,
    pub previous_head: String,
    pub all: bool,
    pub outcome: &'static str,
}

/// The machine result of one snapshot: `snapshot`, `snapshot create`, and each entry of
/// `snapshot list`.
#[derive(Serialize)]
pub struct SnapshotJson<'a> {
    pub workspace: &'a str,
    pub sha: &'a str,
    pub path: &'a Path,
    pub bytes: u64,
}

/// The machine record of one unrepairable worktree link.
pub fn unrepairable_json<'a>(name: &'a str, why: &'a Unrepairable) -> UnrepairableJson<'a> {
    match why {
        Unrepairable::NotAWorktree => UnrepairableJson {
            workspace: name,
            reason: "not_a_worktree",
            message: "not a worktree".to_string(),
            branch: None,
            holder: None,
        },
        Unrepairable::NoBranch => UnrepairableJson {
            workspace: name,
            reason: "no_branch",
            message: "no recorded branch".to_string(),
            branch: None,
            holder: None,
        },
        Unrepairable::MissingBranch(branch) => UnrepairableJson {
            workspace: name,
            reason: "missing_branch",
            message: format!("recorded branch {branch} does not exist"),
            branch: Some(branch),
            holder: None,
        },
        Unrepairable::BranchInUse { branch, holder } => UnrepairableJson {
            workspace: name,
            reason: "branch_in_use",
            message: format!("branch {branch} is checked out in {holder}"),
            branch: Some(branch),
            holder: Some(holder),
        },
        Unrepairable::RebuildFailed(error) => UnrepairableJson {
            workspace: name,
            reason: "rebuild_failed",
            message: format!("rebuild failed: {error}"),
            branch: None,
            holder: None,
        },
    }
}

/// Builds the `list` machine entries for `names`, from the parent edges that `meta` gives.
pub fn list_entries_json<'a>(
    names: &[&'a str],
    edges: &'a HashMap<String, String>,
) -> Vec<ListEntryJson<'a>> {
    names
        .iter()
        .map(|&name| ListEntryJson {
            name,
            parent: edges.get(name).map(String::as_str),
        })
        .collect()
}

#[derive(Serialize)]
pub struct RemoteJson {
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Serialize)]
pub struct ParentJson {
    pub name: String,
    pub commits_behind: usize,
    pub commits_ahead: usize,
    pub rewritten: bool,
    pub replay_base: Option<String>,
}

#[derive(Serialize)]
pub struct OperationJson {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

pub fn operation_json(op: InProgressOperation) -> OperationJson {
    match op {
        InProgressOperation::Rebase { steps } => OperationJson {
            kind: "rebase",
            done: steps.map(|s| s.done),
            total: steps.map(|s| s.total),
        },
        InProgressOperation::Merge => OperationJson {
            kind: "merge",
            done: None,
            total: None,
        },
        InProgressOperation::CherryPick => OperationJson {
            kind: "cherry_pick",
            done: None,
            total: None,
        },
        InProgressOperation::Revert => OperationJson {
            kind: "revert",
            done: None,
            total: None,
        },
    }
}

#[derive(Serialize)]
pub struct FileEntryJson {
    pub path: String,
    pub status: &'static str,
}

#[derive(Serialize)]
pub struct FileCountsJson {
    pub added: usize,
    pub modified: usize,
    pub renamed: usize,
    pub deleted: usize,
    pub untracked: usize,
    pub unmerged: usize,
    pub staged: Vec<FileEntryJson>,
    pub unstaged: Vec<FileEntryJson>,
}

/// One entry of the `list` machine result. The parent is `null` for a root workspace.
#[derive(Serialize)]
pub struct ListEntryJson<'a> {
    pub name: &'a str,
    pub parent: Option<&'a str>,
}

/// One entry of project-scope `show`'s machine result. `status` is `null` when the status of that
/// workspace does not read.
#[derive(Serialize)]
pub struct StatusEntryJson<'a> {
    pub name: &'a str,
    pub status: Option<WorkspaceJson<'a>>,
}

/// One node of the `tree` machine result. `status` is the same object that `show` returns, and it
/// is `null` when the status of that workspace does not read.
#[derive(Serialize)]
pub struct TreeNodeJson<'a> {
    pub name: &'a str,
    pub current: bool,
    pub status: Option<WorkspaceJson<'a>>,
    pub children: Vec<TreeNodeJson<'a>>,
}

#[derive(Serialize)]
pub struct WorkspaceInspection {
    pub project: String,
    pub name: String,
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    pub parent: Option<String>,
    pub root: String,
    pub children: Vec<String>,
    pub state_path: String,
    pub status: WorkspaceJsonOwned,
}

#[derive(Serialize)]
pub struct WorkspaceJsonOwned {
    pub branch: Option<String>,
    pub parent: Option<ParentJson>,
    pub remote: Option<RemoteJson>,
    pub operation: Option<OperationJson>,
    pub files: FileCountsJson,
}

#[derive(Serialize)]
pub struct ProjectInspection {
    pub project: String,
    pub path: String,
    pub config_path: String,
    pub state_path: String,
    pub metadata_config_path: String,
    pub metadata_state_path: String,
    pub default: Option<String>,
    pub branch_prefix: Option<String>,
    pub workspaces: Vec<ListEntryOwned>,
    pub configuration: ConfigurationState,
    pub repair: RepairState,
}

#[derive(Serialize)]
pub struct ListEntryOwned {
    pub name: String,
    pub parent: Option<String>,
}

#[derive(Serialize)]
pub struct ConfigurationState {
    pub config_exists: bool,
    pub state_exists: bool,
}

#[derive(Serialize)]
pub struct RepairState {
    pub needed: bool,
    pub pruned: Vec<String>,
    pub reparented: Vec<ReparentOwned>,
}

#[derive(Serialize)]
pub struct ReparentOwned {
    pub workspace: String,
    pub parent: Option<String>,
}

#[derive(Serialize)]
pub struct WorkspaceComparison {
    pub project: String,
    pub current: String,
    pub workspace: String,
    pub current_head: String,
    pub workspace_head: String,
    pub commits: ComparisonCommits,
    pub statistics: String,
    pub paths: Vec<ComparisonPath>,
    pub dirty_paths: Vec<String>,
    pub rewritten: bool,
}

#[derive(Serialize)]
pub struct ComparisonCommits {
    pub current_only: Vec<Sha>,
    pub workspace_only: Vec<Sha>,
}

#[derive(Serialize)]
pub struct ComparisonPath {
    pub status: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct WorkspaceJson<'a> {
    pub name: &'a str,
    pub branch: Option<&'a str>,
    pub parent: Option<ParentJson>,
    pub remote: Option<RemoteJson>,
    pub operation: Option<OperationJson>,
    pub files: FileCountsJson,
}

fn file_entry_json(path: &Path, status: GitFileStatus) -> FileEntryJson {
    let status = match status {
        GitFileStatus::Added => "added",
        GitFileStatus::Modified => "modified",
        GitFileStatus::Deleted => "deleted",
        GitFileStatus::Untracked => "untracked",
        GitFileStatus::Renamed => "renamed",
        GitFileStatus::Unmerged => "unmerged",
    };
    FileEntryJson {
        path: path.display().to_string(),
        status,
    }
}

pub fn workspace_json<'a>(
    info: &'a WorkspaceInfo,
    meta: &'a ProjectMeta,
    name: &'a str,
) -> WorkspaceJson<'a> {
    let counts = info.git_status.all_counts();
    let count = |status| counts.get(&status).copied().unwrap_or(0);
    WorkspaceJson {
        name,
        branch: meta
            .workspaces
            .get(name)
            .and_then(|ws| ws.branch.as_deref()),
        parent: info.parent.as_ref().map(|p| ParentJson {
            name: p.name.clone(),
            commits_behind: p.commits_behind,
            commits_ahead: p.commits_ahead,
            rewritten: p.rewritten,
            replay_base: p.replay_base.as_ref().map(|sha| sha.sha()),
        }),
        remote: info.remote.map(|r| RemoteJson {
            ahead: r.ahead,
            behind: r.behind,
        }),
        operation: info.operation.map(operation_json),
        files: FileCountsJson {
            added: count(GitFileStatus::Added),
            modified: count(GitFileStatus::Modified),
            renamed: count(GitFileStatus::Renamed),
            deleted: count(GitFileStatus::Deleted),
            untracked: count(GitFileStatus::Untracked),
            unmerged: count(GitFileStatus::Unmerged),
            staged: info
                .git_status
                .index
                .iter()
                .map(|(path, status)| file_entry_json(path, *status))
                .collect(),
            unstaged: info
                .git_status
                .worktree
                .iter()
                .map(|(path, status)| file_entry_json(path, *status))
                .collect(),
        },
    }
}

/// Builds one node of the `tree` machine result, and its subtree. Children sort by name, so the
/// machine result keeps the order that the text tree renders.
pub fn tree_node_json<'a>(
    name: &'a str,
    children_of: &HashMap<&'a str, Vec<&'a str>>,
    info_map: &'a HashMap<String, WorkspaceInfo>,
    meta: &'a ProjectMeta,
    current: Option<&str>,
) -> TreeNodeJson<'a> {
    let mut children: Vec<&str> = children_of.get(name).cloned().unwrap_or_default();
    children.sort_unstable();
    TreeNodeJson {
        name,
        current: current == Some(name),
        status: info_map
            .get(name)
            .map(|info| workspace_json(info, meta, name)),
        children: children
            .into_iter()
            .map(|child| tree_node_json(child, children_of, info_map, meta, current))
            .collect(),
    }
}

/// Builds the `tree` machine nodes for every root of `trees`, and the roots and children that each
/// root derives from its own [`WorkspaceTree`].
pub fn tree_nodes_json<'a>(
    trees: &'a [WorkspaceTree],
    info_map: &'a HashMap<String, WorkspaceInfo>,
    meta: &'a ProjectMeta,
    mark: Option<&str>,
) -> Vec<TreeNodeJson<'a>> {
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut roots: Vec<&str> = Vec::new();
    for tree in trees {
        for node in tree.nodes() {
            match tree.parent_name(node) {
                Some(parent) => children_of.entry(parent).or_default().push(node.name()),
                None => roots.push(node.name()),
            }
        }
    }
    roots
        .into_iter()
        .map(|root| tree_node_json(root, &children_of, info_map, meta, mark))
        .collect()
}

pub fn report_repair_json(
    project: &Project,
    plan: &RepairPlan,
    relinks: &RelinkPlan,
    out: &mut output::Stdio,
    dry_run: bool,
    links_checked: bool,
) -> Result<()> {
    if !out.json() {
        return Ok(());
    }
    out.success(&RepairJson {
        project: project.name(),
        dry_run,
        links_checked,
        relinked: relinks
            .relinks()
            .iter()
            .map(|relink| RelinkJson {
                workspace: &relink.workspace,
                branch: &relink.branch,
            })
            .collect(),
        unrepairable: relinks
            .unrepairable()
            .iter()
            .map(|(name, why)| unrepairable_json(name, why))
            .collect(),
        pruned: plan.pruned().iter().map(String::as_str).collect(),
        reparented: plan
            .reparented()
            .map(|(child, parent)| ReparentJson {
                workspace: child,
                parent,
            })
            .collect(),
        cleared_default: plan.stale_default(),
    })
}
