use subspace::{
    git::{GitFileStatus, GitStatus},
    tree::Tree,
    worktree::InProgressOperation,
};

#[derive(Clone, Copy)]
pub enum Color {
    Red = 31,
    Green = 32,
    Yellow = 33,
    Cyan = 36,
    Dim = 90,
}

pub fn ansi(color: Color, s: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[{}m{}\x1b[0m", color as u8, s)
    } else {
        s.to_string()
    }
}

/// Commit counts relative to a workspace's parent.
pub struct ParentInfo {
    pub commits_behind: usize,
    pub commits_ahead: usize,
}

/// The status fields in a workspace row.
pub struct WorkspaceLabel<'a> {
    pub git_status: &'a GitStatus,
    pub parent: Option<ParentInfo>,
    pub operation: Option<InProgressOperation>,
}

fn op_token(op: &InProgressOperation) -> String {
    match op {
        InProgressOperation::Rebase { steps: Some(s) } => format!("REBASE {}/{}", s.done, s.total),
        InProgressOperation::Rebase { steps: None } => "REBASE".to_string(),
        InProgressOperation::Merge => "MERGE".to_string(),
        InProgressOperation::CherryPick => "PICK".to_string(),
        InProgressOperation::Revert => "REVERT".to_string(),
    }
}

/// Formats a tree row with labeled commit and file counts.
pub fn format_tree_workspace(name: &str, label: &WorkspaceLabel<'_>, color: bool) -> String {
    let mut parts = vec![name.to_string()];
    let mut groups = Vec::new();
    if let Some(parent) = &label.parent {
        let mut counts = Vec::new();
        if parent.commits_behind > 0 {
            counts.push(ansi(
                Color::Red,
                &format!("-{}", parent.commits_behind),
                color,
            ));
        }
        if parent.commits_ahead > 0 {
            counts.push(ansi(
                Color::Green,
                &format!("+{}", parent.commits_ahead),
                color,
            ));
        }
        if !counts.is_empty() {
            groups.push(format!(
                "{}{}",
                ansi(Color::Dim, "commits: ", color),
                counts.join(&ansi(Color::Dim, "/", color))
            ));
        }
    }

    let counts = label.git_status.all_counts();
    let modified = counts.get(&GitFileStatus::Modified).copied().unwrap_or(0)
        + counts.get(&GitFileStatus::Renamed).copied().unwrap_or(0);
    let mut files = Vec::new();
    for (n, tone, symbol) in [
        (modified, Color::Yellow, "~"),
        (
            counts.get(&GitFileStatus::Deleted).copied().unwrap_or(0),
            Color::Red,
            "-",
        ),
        (
            counts.get(&GitFileStatus::Added).copied().unwrap_or(0),
            Color::Green,
            "+",
        ),
        (
            counts.get(&GitFileStatus::Untracked).copied().unwrap_or(0),
            Color::Dim,
            "?",
        ),
        (
            counts.get(&GitFileStatus::Unmerged).copied().unwrap_or(0),
            Color::Red,
            "!",
        ),
    ] {
        if n > 0 {
            files.push(ansi(tone, &format!("{symbol}{n}"), color));
        }
    }
    if !files.is_empty() {
        groups.push(format!(
            "{}{}",
            ansi(Color::Dim, "changes: ", color),
            files.join(&ansi(Color::Dim, "/", color))
        ));
    }
    if !groups.is_empty() {
        parts.push(format!(
            "{}{}{}",
            ansi(Color::Dim, "(", color),
            groups.join(&ansi(Color::Dim, ", ", color)),
            ansi(Color::Dim, ")", color)
        ));
    }
    if let Some(op) = &label.operation {
        parts.push(ansi(Color::Red, &op_token(op), color));
    }
    parts.join(" ")
}

fn file_status_name(status: GitFileStatus) -> &'static str {
    match status {
        GitFileStatus::Added => "added",
        GitFileStatus::Modified => "modified",
        GitFileStatus::Deleted => "deleted",
        GitFileStatus::Untracked => "untracked",
        GitFileStatus::Renamed => "renamed",
        GitFileStatus::Unmerged => "unmerged",
    }
}

/// Formats a workspace summary with every staged and unstaged path.
pub fn format_workspace_details(name: &str, label: &WorkspaceLabel<'_>, color: bool) -> String {
    let mut lines = vec![format_tree_workspace(name, label, color)];
    for (kind, entries) in [
        ("staged", &label.git_status.index),
        ("unstaged", &label.git_status.worktree),
    ] {
        if entries.is_empty() {
            continue;
        }
        lines.push(format!("  {}:", ansi(Color::Dim, kind, color)));
        for (path, status) in entries {
            lines.push(format!(
                "    {}  {}",
                ansi(Color::Dim, file_status_name(*status), color),
                path.display()
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use subspace::worktree::RebaseSteps;

    #[test]
    fn tree_row_labels_commit_and_file_counts() {
        let status = GitStatus {
            index: vec![
                (PathBuf::from("a"), GitFileStatus::Modified),
                (PathBuf::from("b"), GitFileStatus::Renamed),
                (PathBuf::from("c"), GitFileStatus::Added),
            ],
            worktree: vec![
                (PathBuf::from("d"), GitFileStatus::Deleted),
                (PathBuf::from("e"), GitFileStatus::Untracked),
                (PathBuf::from("f"), GitFileStatus::Unmerged),
            ],
        };
        let label = WorkspaceLabel {
            git_status: &status,
            parent: Some(ParentInfo {
                commits_behind: 2,
                commits_ahead: 3,
            }),
            operation: Some(InProgressOperation::Rebase {
                steps: Some(RebaseSteps { done: 1, total: 7 }),
            }),
        };
        assert_eq!(
            format_tree_workspace("child", &label, false),
            "child (commits: -2/+3, changes: ~2/-1/+1/?1/!1) REBASE 1/7"
        );
        let colored = format_tree_workspace("child", &label, true);
        assert!(colored.contains("\x1b[90mcommits: \x1b[0m"));
        assert!(colored.contains("\x1b[90mchanges: \x1b[0m"));
        assert!(colored.contains("\x1b[31m-2\x1b[0m"));
        assert!(colored.contains("\x1b[32m+3\x1b[0m"));
        assert!(colored.contains("\x1b[33m~2\x1b[0m"));
        assert!(colored.contains("\x1b[90m/\x1b[0m"));
    }

    #[test]
    fn tree_row_omits_empty_count_groups() {
        let status = GitStatus::default();
        let root = WorkspaceLabel {
            git_status: &status,
            parent: None,
            operation: None,
        };
        assert_eq!(format_tree_workspace("root", &root, false), "root");
        let child = WorkspaceLabel {
            git_status: &status,
            parent: Some(ParentInfo {
                commits_behind: 0,
                commits_ahead: 1,
            }),
            operation: None,
        };
        assert_eq!(
            format_tree_workspace("child", &child, false),
            "child (commits: +1)"
        );
        let files_only = GitStatus {
            index: vec![(PathBuf::from("a"), GitFileStatus::Added)],
            worktree: vec![],
        };
        let root = WorkspaceLabel {
            git_status: &files_only,
            parent: None,
            operation: None,
        };
        assert_eq!(
            format_tree_workspace("root", &root, false),
            "root (changes: +1)"
        );
    }
}

/// Renders a tree with box-drawing connectors.
pub fn render_tree<T: Tree>(
    tree: &T,
    sort_key: &impl Fn(&T::Node) -> String,
    label: &impl Fn(&T::Node) -> String,
) -> String {
    let mut output = String::new();
    render_node(&mut output, tree.root(), "", None, tree, sort_key, label);
    output
}

fn render_node<T: Tree>(
    output: &mut String,
    node: &T::Node,
    prefix: &str,
    connector: Option<(&str, &str)>,
    tree: &T,
    sort_key: &impl Fn(&T::Node) -> String,
    label: &impl Fn(&T::Node) -> String,
) {
    let child_prefix = match connector {
        Some((connector, extension)) => {
            output.push_str(&format!("{}{}{}\n", prefix, connector, label(node)));
            format!("{}{}", prefix, extension)
        }
        None => {
            output.push_str(&format!("{}\n", label(node)));
            prefix.to_string()
        }
    };
    let mut children = tree.children(node);
    children.sort_by_key(|n| sort_key(n));
    for (i, child) in children.iter().enumerate() {
        let last = i == children.len() - 1;
        let connector = if last { "└── " } else { "├── " };
        let extension = if last { "    " } else { "│   " };
        render_node(
            output,
            *child,
            &child_prefix,
            Some((connector, extension)),
            tree,
            sort_key,
            label,
        );
    }
}
