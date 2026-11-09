use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, IsTerminal, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process,
};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use subspace::{
    client::{ClientSettings, Verbosity},
    error::Error,
    git::GitFileStatus,
    hook::{Fields, HookRunner, Kind as HookKind, Phase, Stdout as HookStdout, run_post_hooks},
    metadata::{IntegrationState, ParentEdge, ProjectMeta, WorkspaceMeta},
    paths,
    project::{
        Project, ProjectLock, create_project, move_target_path, rename_target_path,
        validate_project_name,
    },
    registry::{GlobalState, Registry},
    revision::{Revision, Sha, StartPoint},
    scope::{Context, Scope, TargetScope},
    snapshot,
    workspace::{
        RebaseIo, RebaseMode, RebaseOutcome, RelinkPlan, RepairPlan, ReparentPlan, Workspace,
        WorkspaceInfo, WorkspaceTree, apply_relinks, plan_insert_parent, plan_relinks, plan_repair,
        plan_reparents, require_merge_chains_current, validate_name,
    },
    worktree::{self, InProgressOperation, Worktree},
};

mod format;
mod machine;
mod output;

#[derive(Parser)]
#[command(
    name = "subspace",
    version,
    about = "Manage workspaces backed by git worktrees"
)]
struct Cli {
    /// Target a specific project by name.
    #[arg(short = 'P', long = "project", global = true)]
    project_name: Option<String>,

    /// Use CWD's project without selecting a workspace.
    #[arg(short = 'p', global = true)]
    project_scope: bool,

    /// Target a specific workspace by name or @sigil.
    #[arg(short = 'w', long = "workspace", global = true)]
    workspace_name: Option<String>,

    /// When to use color output.
    #[arg(long, global = true, default_value = "auto")]
    color: ColorChoice,

    /// Prompt for confirmation (default: detect a terminal on stdin).
    #[arg(long, global = true, overrides_with = "no_interactive")]
    interactive: bool,

    /// Never prompt for confirmation.
    #[arg(long = "no-interactive", global = true, overrides_with = "interactive")]
    no_interactive: bool,

    /// Suppress informational output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Emit additional diagnostic output.
    #[arg(short, long, global = true, conflicts_with = "quiet")]
    verbose: bool,

    /// Reserve stdout for one machine JSON document and move human output to stderr.
    #[arg(long, global = true)]
    json: bool,

    /// Report the resolved mutation plan without changing state.
    #[arg(long, global = true)]
    preview: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorChoice {
    Always,
    Never,
    Auto,
}

/// The three-state color option of the library for a `--color` choice. The library detects `auto`
/// against the terminal itself.
fn color_flag(color: ColorChoice) -> Option<bool> {
    match color {
        ColorChoice::Always => Some(true),
        ColorChoice::Never => Some(false),
        ColorChoice::Auto => None,
    }
}

impl Cli {
    /// Maps the global flags to resolved [`ClientSettings`]. The library detects an `auto` option,
    /// and an option that no flag sets, against the given terminal states. The caller passes the
    /// real detection so tests can inject fixed states instead of the terminal of the test process.
    fn settings(&self, stdout_terminal: bool, stderr_terminal: bool) -> ClientSettings {
        let color = match (self.color, output::auto_color_stream_is_stderr(self.json)) {
            (ColorChoice::Auto, true) => Some(stderr_terminal),
            (ColorChoice::Auto, false) => Some(stdout_terminal),
            (choice, _) => color_flag(choice),
        };
        let interactive = if self.interactive {
            Some(true)
        } else if self.no_interactive {
            Some(false)
        } else {
            None
        };
        ClientSettings::resolve(color, interactive, self.verbosity())
    }

    /// The verbosity that the global flags select.
    fn verbosity(&self) -> Verbosity {
        if self.quiet {
            Verbosity::Quiet
        } else if self.verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }
}

#[derive(Subcommand, Serialize)]
enum Cmd {
    /// Report the machine protocol identity.
    Capabilities,
    /// Print the current workspace name.
    Current,
    /// Print the workspace or project path.
    Path,
    /// Project subcommands.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Print the root workspace name.
    Root,
    /// Print the parent workspace name.
    Parent,
    /// Print or set the project's default workspace.
    Default {
        /// Workspace to set as the default. Without it, print the current default.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        set: Option<String>,
    },
    /// Print or set the project's branch prefix.
    BranchPrefix {
        /// New branch prefix. Without it, print the current prefix.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        set: Option<String>,
        /// Clear the branch prefix.
        #[arg(long, conflicts_with = "set")]
        unset: bool,
    },
    /// Print or set the branch recorded for workspace repair.
    WorkspaceBranch {
        /// Branch to record. Without it, print the recorded branch.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        set: Option<String>,
        /// Clear the recorded branch.
        #[arg(long, conflicts_with = "set")]
        unset: bool,
    },
    /// Print names of direct child workspaces.
    Children,
    /// Print names of sibling workspaces.
    Siblings,
    /// Print workspace names in the current project, one per line.
    #[command(alias = "ls")]
    List {
        /// Print "name<TAB>parent" per workspace. The parent is empty for a root.
        #[arg(long)]
        parents: bool,
    },
    /// Print workspace summaries and staged and unstaged paths as a flat list.
    Show,
    /// Inspect one workspace or project as a deterministic machine record.
    Inspect {
        #[command(subcommand)]
        target: InspectTarget,
    },
    /// Compare the current workspace with another workspace.
    Compare {
        /// Workspace name or @sigil to compare with the current workspace.
        workspace: String,
    },
    /// Print the workspace tree with status indicators.
    #[command(alias = "st")]
    Tree,
    /// Run `git diff` with a merge-base range, expanding workspace names and @sigils.
    #[command(trailing_var_arg = true)]
    Diff {
        #[command(flatten)]
        range: RangeArgs,
        /// A bare workspace or @sigil expands to `<target>...@current`.
        /// `A..B` and `A...B` pass through. More `git diff` arguments follow.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run `git log` oldest first with a symmetric range, expanding workspace names and @sigils.
    #[command(trailing_var_arg = true)]
    Log {
        #[command(flatten)]
        range: RangeArgs,
        /// A bare workspace or @sigil expands to `<target>...@current`.
        /// `A..B` and `A...B` pass through. More `git log` arguments follow.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Add a new workspace from a git ref or another workspace.
    Add {
        #[command(flatten)]
        #[serde(flatten)]
        args: AddArgs,
        /// Parent workspace name or @sigil (default: none, a root workspace).
        #[arg(long)]
        parent: Option<String>,
        /// Insert the new workspace as the parent of CHILD, and anchor CHILD onto it. CHILD is not
        /// rebased. The new workspace takes the former parent of CHILD.
        #[arg(
            long,
            value_name = "CHILD",
            conflicts_with = "parent",
            conflicts_with = "git_ref"
        )]
        parent_of: Option<String>,
    },
    /// Add a new workspace under the current one. It is an alias for `add --parent=@current`.
    Branch {
        #[command(flatten)]
        #[serde(flatten)]
        args: AddArgs,
    },
    /// Remove a workspace worktree.
    Remove {
        /// Workspace name, project/workspace, or @sigil.
        target: String,
        /// Keep the git branch after removing the worktree.
        #[arg(long)]
        keep_branch: bool,
        /// Remove workspace and all descendants.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Rename a workspace.
    Rename {
        /// Workspace name, project/workspace, or @sigil.
        target: String,
        /// New workspace name.
        new_name: String,
    },
    /// Repair a project. It prunes the dead metadata, and corrects or rebuilds a broken worktree
    /// link.
    Repair {
        /// Project name (default: the scoped project).
        project: Option<String>,
        /// Report what changes, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge a workspace into the current one.
    Merge {
        /// Source workspace name or @sigil.
        source: String,
        /// Squash the selected commits with this commit message.
        #[arg(short = 'm', long)]
        squash: Option<String>,
        /// Replay every source commit since the common ancestor with the destination.
        #[arg(long)]
        all: bool,
        /// Also bring unstaged changes and untracked files.
        #[arg(long)]
        unstaged: bool,
        /// Remove the source workspace after a successful merge.
        #[arg(long)]
        remove: bool,
        /// Delete the source branch when removing the source workspace.
        #[arg(long)]
        delete_branch: bool,
    },
    /// Rebase workspace onto a base workspace.
    Rebase {
        /// Base workspace name or @sigil (default: @parent).
        base: Option<String>,
        /// Rebase interactively (git rebase -i).
        #[arg(id = "rebase_interactive", short = 'i')]
        interactive: bool,
        /// Replay every commit ahead of the base, not only those ahead of the branch point. It
        /// keeps the commits that the base rewrote below the branch point.
        #[arg(long)]
        all: bool,
    },
    /// Continue or abort a stopped merge or rebase.
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Archive a workspace's commit, list snapshots, or restore one.
    Snapshot {
        #[command(subcommand)]
        action: Option<SnapshotAction>,
    },
}

#[derive(Subcommand, Serialize)]
enum InspectTarget {
    /// Inspect the current workspace.
    Workspace,
    /// Inspect the current project.
    Project,
}

#[derive(Subcommand, Serialize)]
enum IntegrationAction {
    /// Continue a stopped merge or rebase.
    Continue,
    /// Abort a stopped merge or rebase.
    Abort,
}

#[derive(Args, Serialize)]
struct AddArgs {
    /// Name for the new workspace.
    name: String,
    /// Start point: a git ref or @sigil (default: the HEAD of @current).
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    /// New branch name (default: from the workspace name). Without it, when the ref names an
    /// existing local branch, that branch is checked out and no new branch is created.
    #[arg(long, conflicts_with = "detach")]
    branch: Option<String>,
    /// Check out the start point with a detached HEAD, on no branch.
    #[arg(long)]
    detach: bool,
    /// Carry unstaged changes and untracked files from the parent workspace.
    #[arg(long)]
    unstaged: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
enum OutputMode {
    Patch,
    Stat,
    NameOnly,
}

#[derive(Args, Serialize)]
struct RangeArgs {
    /// Git output mode.
    #[arg(long, value_enum)]
    output: Option<OutputMode>,
    /// Repository-relative paths to include.
    #[arg(long = "path")]
    paths: Vec<String>,
    /// Range from @parent to @current.
    #[arg(long, conflicts_with_all = ["root", "in_ws", "from", "to"])]
    parent: bool,
    /// Range from @root to @current.
    #[arg(long, conflicts_with_all = ["parent", "in_ws", "from", "to"])]
    root: bool,
    /// Range from @parent to <WS>.
    #[arg(
        long = "in",
        value_name = "WS",
        conflicts_with_all = ["parent", "root", "from", "to"]
    )]
    in_ws: Option<String>,
    /// Range from <WS> to @current.
    #[arg(
        long,
        value_name = "WS",
        conflicts_with_all = ["parent", "root", "in_ws"]
    )]
    from: Option<String>,
    /// Range from @current to <WS>.
    #[arg(
        long,
        value_name = "WS",
        conflicts_with_all = ["parent", "root", "in_ws"]
    )]
    to: Option<String>,
}

impl RangeArgs {
    /// Whether any of the five range flags is set.
    fn is_set(&self) -> bool {
        self.parent || self.root || self.in_ws.is_some() || self.from.is_some() || self.to.is_some()
    }
}

#[derive(Subcommand, Serialize)]
enum ProjectAction {
    /// Print the current project name.
    Current,
    /// Deregister a project. It deletes a file only with `--remove-directory`.
    Remove {
        /// Project name.
        name: String,
        /// Also delete the project directory from disk.
        #[arg(long)]
        remove_directory: bool,
    },
    /// Rename a project. It renames the registry entry, and the directory too unless `--no-move`.
    Rename {
        /// Project name.
        name: String,
        /// New project name.
        new_name: String,
        /// Leave the project directory where it is, so its basename and the project name differ.
        #[arg(long)]
        no_move: bool,
    },
    /// Move the directory of a project. The registry entry follows it. The name stays, and so do
    /// the config and state directories.
    Move {
        /// Project name.
        name: String,
        /// Directory to move the project into, on the same filesystem. The project lands at
        /// `<dir>/<name>`.
        dir: String,
    },
    /// List all project names.
    List,
    /// Clone a remote repository into a new project.
    Clone {
        /// Remote URL to clone.
        remote: String,
        /// Directory to create the project in (default: the CWD). The project lands at
        /// `<dir>/<name>`.
        dir: Option<String>,
        /// Name for the project (default: derived from the remote URL).
        #[arg(long)]
        name: Option<String>,
    },
    /// Initialize a new blank repository as a workspace project, with no initial commit.
    Init {
        /// Path to the new project directory.
        path: String,
        /// Project name (default: the path basename).
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand, Serialize)]
enum SnapshotAction {
    /// Archive a commit into the snapshots directory of the project. The default action.
    Create {
        /// Git ref or @sigil to archive (default: the HEAD of the workspace).
        #[serde(rename = "ref")]
        git_ref: Option<String>,
    },
    /// List the snapshots of the project.
    List,
    /// Create a workspace at the commit of a snapshot.
    Restore {
        /// Snapshot path, or a file name in the snapshots directory of the project.
        file: String,
        /// Name for the new workspace (default: the workspace of the snapshot).
        name: Option<String>,
    },
}

#[derive(Serialize)]
enum ScopeInfo<'a> {
    Project { name: &'a str },
    Workspace { project: &'a str, name: &'a str },
}

#[derive(Serialize)]
struct LockHolder<'a> {
    scope: ScopeInfo<'a>,
    command: &'a Cmd,
}

fn lock_holder(scope: &Scope, cmd: &Cmd) -> String {
    let scope_info = match scope {
        Scope::Project(p) => ScopeInfo::Project { name: p.name() },
        Scope::Workspace(ws) => ScopeInfo::Workspace {
            project: ws.project().name(),
            name: ws.name(),
        },
    };
    let holder = LockHolder {
        scope: scope_info,
        command: cmd,
    };
    serde_json::to_string(&holder).unwrap_or_else(|_| "subspace".to_string())
}

fn resolve_absolute_target(state: &GlobalState, target: &str) -> Result<Workspace> {
    if target.starts_with('@') {
        bail!("sigil `{target}` requires a workspace context");
    }
    if let Some((project_name, ws_name)) = target.split_once('/') {
        let project = Project::get(state, project_name)?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_name}"))?;
        Workspace::get(&project, ws_name)
            .ok_or_else(|| anyhow::anyhow!("workspace not found: {target}"))
    } else {
        bail!("cannot resolve target outside a project: {target}")
    }
}

/// Resolves `target`, which is a workspace name, a `project/workspace`, or an @sigil, against the
/// context of the CWD. With no context, it resolves the target on its own.
fn resolve_workspace(state: &GlobalState, cli: &Cli, target: &str) -> Result<Workspace> {
    Ok(match try_resolve_context(state, cli)? {
        Some(ctx) => ctx.resolve_target(state, target, TargetScope::Any)?,
        None => resolve_absolute_target(state, target)?,
    })
}

fn try_resolve_context(state: &GlobalState, cli: &Cli) -> Result<Option<Context>> {
    let cwd = std::env::current_dir()?;
    let project_name = if cli.project_scope {
        Some("")
    } else {
        cli.project_name.as_deref()
    };
    Ok(Scope::resolve(
        state,
        project_name,
        cli.workspace_name.as_deref(),
        &cwd,
    )?)
}

fn resolve_context(state: &GlobalState, cli: &Cli) -> Result<Context> {
    match try_resolve_context(state, cli)? {
        Some(ctx) => Ok(ctx),
        None => {
            if cli.workspace_name.is_some() {
                bail!("workspace not found");
            } else if cli.project_name.is_some() || cli.project_scope {
                bail!("project not found");
            } else {
                bail!("context is not a project or workspace");
            }
        }
    }
}

/// Whether raw argv asks for the JSON error document on a clap usage error. This checks argv
/// directly, because a usage error means [`Cli`] never parsed, so the `--json` flag is not
/// available on it. The scan stops at the first `--`, so a flag on a nested command, such as
/// An argument after the delimiter does not switch the usage error to JSON.
fn wants_json_usage_error(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--json")
}

/// Writes the machine error document of a clap usage error to `out`, and returns the process
/// status. `--json` alone can fail to parse (an unknown flag, a missing value), so the caller
/// detects it from raw argv rather than from the unparsed [`Cli`].
fn report_usage_error<R: io::Write, D: io::Write, M: io::Write>(
    out: &mut output::Channel<R, D, M>,
    err: &clap::Error,
) -> i32 {
    out.failure("usage", err.to_string().trim_end());
    out.finish()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => {
            match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    e.exit()
                }
                _ => {}
            }
            if wants_json_usage_error(&args) {
                let mut out = output::Channel::stdio(true, Verbosity::Normal);
                process::exit(report_usage_error(&mut out, &e));
            }
            e.exit();
        }
    };
    let mut out = output::Channel::stdio(cli.json, cli.verbosity());
    if cli.json && !json_supported(&cli.command) {
        let (code, message) = json_rejection(&cli.command);
        out.failure(code, message);
        process::exit(out.finish());
    }
    if let Err(e) = run(&cli, &mut out) {
        // `{:#}` makes an anyhow context chain report its cause too. Without it, a wrapped error
        // prints the outer layer alone, which is the half that says least about the failure.
        out.failure("failed", &format!("{e:#}"));
    }
    process::exit(out.finish());
}

/// Whether a command produces a machine document for the global JSON channel. The later steps of
/// the JSON migration add one operation group at a time.
fn json_supported(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Capabilities
            | Cmd::Current
            | Cmd::Path
            | Cmd::Root
            | Cmd::Parent
            | Cmd::Children
            | Cmd::Siblings
            | Cmd::List { .. }
            | Cmd::Show
            | Cmd::Inspect { .. }
            | Cmd::Compare { .. }
            | Cmd::Tree
            | Cmd::Diff { .. }
            | Cmd::Log { .. }
            | Cmd::Default { .. }
            | Cmd::BranchPrefix { .. }
            | Cmd::WorkspaceBranch { .. }
            | Cmd::Project { .. }
            | Cmd::Add { .. }
            | Cmd::Branch { .. }
            | Cmd::Remove { .. }
            | Cmd::Rename { .. }
            | Cmd::Repair { .. }
            | Cmd::Merge { .. }
            | Cmd::Rebase {
                interactive: false,
                ..
            }
            | Cmd::Integration { .. }
            | Cmd::Snapshot { .. }
    )
}

/// The error code and message of a command that the JSON channel refuses. A creation action that
/// runs in the foreground attaches a session, so it holds the terminal and never returns a machine
/// result. The background flag is the fix, and the message says so.
fn json_rejection(cmd: &Cmd) -> (&'static str, &'static str) {
    match cmd {
        Cmd::Rebase {
            interactive: true, ..
        } => (
            "unsupported",
            "an interactive rebase holds the terminal, so it has no JSON result",
        ),
        _ => ("unsupported", "the command has no JSON result"),
    }
}

fn mutation_action(cmd: &Cmd) -> Option<&'static str> {
    match cmd {
        Cmd::Default { set } if set.is_some() => Some("default"),
        Cmd::BranchPrefix { set, unset } if set.is_some() || *unset => Some("branch-prefix"),
        Cmd::WorkspaceBranch { set, unset } if set.is_some() || *unset => Some("workspace-branch"),
        Cmd::Project { action } => match action {
            ProjectAction::Remove { .. } => Some("project-remove"),
            ProjectAction::Rename { .. } => Some("project-rename"),
            ProjectAction::Move { .. } => Some("project-move"),
            ProjectAction::Clone { .. } => Some("project-clone"),
            ProjectAction::Init { .. } => Some("project-init"),
            _ => None,
        },
        Cmd::Add { .. } | Cmd::Branch { .. } => Some("add"),
        Cmd::Remove { .. } => Some("remove"),
        Cmd::Rename { .. } => Some("rename"),
        Cmd::Repair { dry_run: false, .. } => Some("repair"),
        Cmd::Merge { .. } => Some("merge"),
        Cmd::Rebase { .. } => Some("rebase"),
        Cmd::Integration { .. } => Some("integration"),
        Cmd::Snapshot { action } => match action {
            None | Some(SnapshotAction::Create { .. }) => Some("snapshot-create"),
            Some(SnapshotAction::Restore { .. }) => Some("snapshot-restore"),
            Some(SnapshotAction::List) => None,
        },
        _ => None,
    }
}

fn mutation_effects(state: &GlobalState, cli: &Cli, action: &str) -> Result<serde_json::Value> {
    let command = serde_json::to_value(&cli.command)?;
    match &cli.command {
        Cmd::Default { set: Some(name) } => {
            let ctx = resolve_context(state, cli)?;
            let project = ctx.scope.project();
            let old = project.default_workspace()?.map(|ws| ws.name().to_string());
            let target = Workspace::get(project, name)
                .ok_or_else(|| anyhow::anyhow!("workspace not found: {name}"))?;
            Ok(
                serde_json::json!({"kind":"setting", "setting":"default", "project":project.name(), "from":old, "to":target.name()}),
            )
        }
        Cmd::BranchPrefix { set, unset } if set.is_some() || *unset => {
            let ctx = resolve_context(state, cli)?;
            let project = ctx.scope.project();
            Ok(
                serde_json::json!({"kind":"setting", "setting":"branch_prefix", "project":project.name(), "from":project.branch_prefix()?, "to":if *unset { None } else { set.clone() }}),
            )
        }
        Cmd::WorkspaceBranch { set, unset } if set.is_some() || *unset => {
            let ctx = resolve_context(state, cli)?;
            let ws = ctx.scope.require_workspace()?;
            let meta = ws.project().metadata().read()?;
            Ok(
                serde_json::json!({"kind":"setting", "setting":"workspace_branch", "project":ws.project().name(), "workspace":ws.name(), "from":meta.workspaces.get(ws.name()).and_then(|entry| entry.branch.as_deref()), "to":if *unset { None } else { set.as_deref() }}),
            )
        }
        Cmd::Project { action } => {
            let effect = match action {
                ProjectAction::Remove {
                    name,
                    remove_directory,
                } => {
                    let path = state
                        .get(name)
                        .ok_or_else(|| Error::ProjectNotFound(name.clone()))?;
                    serde_json::json!({"operation":"remove", "name":name, "path":path, "remove_directory":remove_directory, "config":paths::project_config_dir(name)?, "state":paths::project_state_dir(name)?})
                }
                ProjectAction::Rename {
                    name,
                    new_name,
                    no_move,
                } => {
                    let old = registered_project(state, name)?;
                    let path = if *no_move {
                        old.path().to_path_buf()
                    } else {
                        rename_target_path(&old, new_name)?
                    };
                    serde_json::json!({"operation":"rename", "from":name, "to":new_name, "old_path":old.path(), "new_path":path, "move":!no_move})
                }
                ProjectAction::Move { name, dir } => {
                    let old = registered_project(state, name)?;
                    serde_json::json!({"operation":"move", "name":name, "old_path":old.path(), "new_path":move_target_path(&old, &PathBuf::from(dir).canonicalize()?)?})
                }
                ProjectAction::Clone { remote, dir, name } => {
                    let name = name.as_deref().unwrap_or_else(|| derive_clone_name(remote));
                    serde_json::json!({"operation":"clone", "remote":remote, "name":name, "path":project_parent_dir(dir.as_deref())?.join(name)})
                }
                ProjectAction::Init { path, name } => {
                    let path = PathBuf::from(path);
                    let name = project_init_name(&path, name.as_deref())?;
                    serde_json::json!({"operation":"init", "name":name, "path":path})
                }
                _ => unreachable!(),
            };
            Ok(serde_json::json!({"kind":"project", "project":effect}))
        }
        Cmd::Add { args, .. } | Cmd::Branch { args } => {
            let ctx = resolve_context(state, cli)?;
            let project = ctx.scope.project();
            let (parent, parent_of) = match &cli.command {
                Cmd::Add {
                    parent, parent_of, ..
                } => (parent.as_deref(), parent_of.as_deref()),
                _ => (Some("@current"), None),
            };
            let meta = project.metadata().read()?;
            let parent_name = if let Some(child) = parent_of {
                ctx.resolve_target(state, child, TargetScope::SameProject)?
                    .parent_workspace(&meta)
                    .map(|ws| ws.name().to_string())
            } else {
                parent
                    .map(|p| ctx.resolve_target(state, p, TargetScope::SameProject))
                    .transpose()?
                    .map(|ws| ws.name().to_string())
            };
            let child = parent_of
                .map(|s| ctx.resolve_target(state, s, TargetScope::SameProject))
                .transpose()?;
            Ok(
                serde_json::json!({"kind":"workspace", "operation":"add", "project":project.name(), "name":args.name, "path":Workspace::new(project.clone(), args.name.clone()).path(), "parent":parent_name, "parent_of":child.map(|ws| ws.name().to_string()), "ref":args.git_ref, "branch":args.branch, "detach":args.detach, "unstaged":args.unstaged}),
            )
        }
        Cmd::Remove {
            target,
            keep_branch,
            recursive,
        } => {
            let ws = resolve_workspace(state, cli, target)?;
            let project = ws.project();
            let targets = if *recursive {
                WorkspaceTree::from_workspace(&ws)?.post_order(&ws)
            } else {
                vec![ws.clone()]
            };
            let meta = project.metadata().read()?;
            let plan = plan_reparents(&targets, &meta)?;
            Ok(
                serde_json::json!({"kind":"workspace", "operation":"remove", "project":project.name(), "targets":targets.iter().map(|ws| serde_json::json!({"name":ws.name(),"path":ws.path(),"branch":ws.worktree().ok().and_then(|wt| wt.get_branch())})).collect::<Vec<_>>(), "delete_branch":!*keep_branch, "reparented":plan.reparented().map(|(child,parent)| serde_json::json!({"workspace":child,"parent":parent})).collect::<Vec<_>>() }),
            )
        }
        Cmd::Rename { target, new_name } => {
            let ws = resolve_workspace(state, cli, target)?;
            let project = ws.project();
            let meta = project.metadata().read()?;
            Ok(
                serde_json::json!({"kind":"workspace","operation":"rename","project":project.name(),"from":ws.name(),"to":new_name,"old_path":ws.path(),"new_path":Workspace::new(project.clone(),new_name.clone()).path(),"branch":ws.worktree()?.get_branch(),"parent":ws.parent_workspace(&meta).map(|p|p.name().to_string())}),
            )
        }
        Cmd::Repair {
            project: name,
            dry_run: false,
        } => {
            let p = match name.as_deref() {
                Some(name) => registered_project(state, name)?,
                None => resolve_context(state, cli)?.scope.project().clone(),
            };
            let meta = p.metadata().read()?;
            let plan = plan_repair(&p, &meta)?;
            Ok(
                serde_json::json!({"kind":"workspace","operation":"repair","project":p.name(),"dry_run":false,"pruned":plan.pruned(),"reparented":plan.reparented().collect::<Vec<_>>() }),
            )
        }
        Cmd::Snapshot { action } => {
            let ctx = resolve_context(state, cli)?;
            let project = ctx.scope.project().clone();
            snapshot_effects(state, cli, &project, action.as_ref())
        }
        Cmd::Merge { .. } | Cmd::Rebase { .. } => {
            unreachable!("integration previews use integration_preview")
        }
        _ => Ok(serde_json::json!({"kind":action,"command":command})),
    }
}

fn snapshot_effects(
    state: &GlobalState,
    cli: &Cli,
    project: &Project,
    action: Option<&SnapshotAction>,
) -> Result<serde_json::Value> {
    let ws = ctx_workspace(state, cli)?;
    match action {
        None | Some(SnapshotAction::Create { git_ref: None }) => Ok(
            serde_json::json!({"kind":"snapshot","operation":"create","workspace":ws.name(),"ref":"HEAD","resolved":ws.worktree()?.resolve_ref("HEAD")?.sha(),"path":project.snapshots_path()}),
        ),
        Some(SnapshotAction::Create {
            git_ref: Some(reference),
        }) => Ok(
            serde_json::json!({"kind":"snapshot","operation":"create","workspace":ws.name(),"ref":reference,"resolved":ws.worktree()?.resolve_ref(reference)?.sha()}),
        ),
        Some(SnapshotAction::Restore { file, name }) => {
            let snapshot = snapshot::find(project, file)?;
            let commit = snapshot.commit(project)?;
            Ok(
                serde_json::json!({"kind":"snapshot","operation":"restore","file":file,"name":name.as_deref().unwrap_or(snapshot.workspace()),"ref":commit.sha()}),
            )
        }
        Some(SnapshotAction::List) => unreachable!(),
    }
}

fn ctx_workspace(state: &GlobalState, cli: &Cli) -> Result<Workspace> {
    Ok(resolve_context(state, cli)?
        .scope
        .require_workspace()?
        .clone())
}

fn mutation_confirmation(action: &str) -> String {
    format!("Apply the resolved {action} change?")
}

fn mutation_warnings(_action: &str) -> Vec<String> {
    Vec::new()
}

fn cmd_mutation_preview(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    action: &str,
) -> Result<()> {
    let effects = mutation_effects(state, cli, action)?;
    out.success(&machine::IntegrationPreview {
        plan: machine::IntegrationPlan {
            effects,
            warnings: mutation_warnings(action),
            confirmation: mutation_confirmation(action),
            applied_commits: Vec::new(),
            dropped_commits: Vec::new(),
            anchors: Vec::new(),
            changed_paths: Vec::new(),
            overlap_paths: Vec::new(),
            requested_unstaged_paths: Vec::new(),
            untracked_collisions: Vec::new(),
            stopped: false,
        },
    })?;
    Ok(())
}

fn integration_paths(status: &subspace::git::GitStatus) -> Vec<String> {
    let mut paths: Vec<String> = status
        .index
        .iter()
        .chain(status.worktree.iter())
        .map(|(path, _)| path.display().to_string())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn common_paths(left: &[String], right: &[String]) -> Vec<String> {
    let right: HashSet<&str> = right.iter().map(String::as_str).collect();
    left.iter()
        .filter(|path| right.contains(path.as_str()))
        .cloned()
        .collect()
}

fn committed_paths(wt: &Worktree, commits: &[Sha]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for commit in commits {
        paths.extend(wt.revision_paths(commit)?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn cli_unstaged(command: &Cmd) -> bool {
    matches!(command, Cmd::Merge { unstaged: true, .. })
}

fn merge_commits(
    wt: &Worktree,
    dest: &impl Revision,
    source: &impl Revision,
    edge: Option<&subspace::metadata::ParentEdge>,
    all: bool,
) -> Result<(Vec<Sha>, Vec<Sha>, Sha)> {
    let anchor = if all {
        wt.merge_base(dest, source)?
    } else {
        let edge = edge.ok_or_else(|| anyhow::anyhow!("source has no parent; use --all"))?;
        if !wt.is_ancestor(&edge.base, source)? {
            bail!("source fork point is not in its history");
        }
        edge.base
    };
    let candidates = wt.list_revisions(&anchor, source)?;
    let unique: HashSet<Sha> = wt
        .list_unique_revisions(dest, source)?
        .into_iter()
        .collect();
    let (applied, dropped) = candidates.into_iter().partition(|sha| unique.contains(sha));
    Ok((applied, dropped, anchor))
}

fn integration_preview(state: &GlobalState, cli: &Cli) -> Result<machine::IntegrationPreview> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let project = ctx.scope.project();
    let meta = project.metadata().read()?;
    let wt = ws.worktree()?;
    let applied: Vec<String>;
    let dropped: Vec<String>;
    let mut anchors = Vec::new();
    let changed: Vec<String>;
    let mut overlap: Vec<String>;
    let mut requested = Vec::new();
    let mut collisions = Vec::new();
    let mut stopped = false;
    let summary;
    match &cli.command {
        Cmd::Merge { source, all, .. } => {
            let source_ws = ctx.resolve_target(state, source, TargetScope::SameProject)?;
            let base = wt.head()?;
            let source_wt = source_ws.worktree()?;
            let source_head = source_wt.head()?;
            require_merge_chains_current(ws, &source_ws, &meta)?;
            let (selected, skipped, anchor) = merge_commits(
                &wt,
                &base,
                &source_head,
                source_ws.parent_edge(&meta).as_ref(),
                *all,
            )?;
            applied = selected.iter().map(|sha| sha.sha()).collect();
            dropped = skipped.iter().map(|sha| sha.sha()).collect();
            let source_status = source_wt.status()?;
            let destination_status = wt.status()?;
            if cli_unstaged(&cli.command) {
                requested = integration_paths(&source_status);
            }
            let destination_dirty = integration_paths(&destination_status);
            let merge_base = wt.merge_base(&base, &source_head)?;
            let destination_commits = wt.list_revisions(&merge_base, &base)?;
            let mut source_candidates = selected.clone();
            source_candidates.extend(skipped);
            let source_committed = committed_paths(&wt, &source_candidates)?;
            let destination_committed = committed_paths(&wt, &destination_commits)?;
            overlap = common_paths(&source_committed, &destination_committed);
            overlap.extend(common_paths(&requested, &destination_dirty));
            overlap.sort();
            overlap.dedup();
            let destination_untracked: HashSet<String> = destination_status
                .worktree
                .iter()
                .filter(|(_, status)| *status == GitFileStatus::Untracked)
                .map(|(path, _)| path.display().to_string())
                .collect();
            let destination_tracked: HashSet<String> =
                wt.tracked_paths(&base)?.into_iter().collect();
            collisions = source_status
                .worktree
                .iter()
                .filter_map(|(path, status)| {
                    let value = path.display().to_string();
                    (status == &GitFileStatus::Untracked
                        && (destination_untracked.contains(&value)
                            || destination_tracked.contains(&value)))
                    .then_some(value)
                })
                .collect();
            changed = committed_paths(&wt, &selected)?;
            anchors.push(anchor.sha());
            summary = format!("merge {} into {}", source_ws.name(), ws.name());
        }
        Cmd::Rebase {
            base,
            interactive,
            all,
        } => {
            let former = ws.parent_workspace(&meta);
            let onto = match base {
                Some(name) => ctx.resolve_target(state, name, TargetScope::SameProject)?,
                None => former.ok_or_else(|| {
                    anyhow::anyhow!("no base specified and workspace has no parent")
                })?,
            };
            let onto_head = onto.worktree()?.head()?;
            let head = wt.head()?;
            let plan = ws.plan_rebase(
                &wt,
                &meta,
                &onto,
                &onto_head,
                &head,
                RebaseMode {
                    interactive: *interactive,
                    all: *all,
                },
            )?;
            let replay_base = match &plan {
                subspace::workspace::RebasePlan::SkipReplay => onto_head.commit(),
                subspace::workspace::RebasePlan::Replay { branch_point } => {
                    (*branch_point).map_or_else(|| wt.merge_base(&onto_head, &head), Ok)?
                }
            };
            let replayed = wt.list_revisions(&replay_base, &head)?;
            let unique = wt.list_unique_revisions(&onto_head, &head)?;
            applied = replayed.iter().map(|sha| sha.sha()).collect();
            dropped = replayed
                .iter()
                .filter(|sha| !unique.iter().any(|item| item == *sha))
                .map(|sha| sha.sha())
                .collect();
            changed = committed_paths(&wt, &replayed)?;
            let merge_base = wt.merge_base(&onto_head, &head)?;
            let onto_commits = wt.list_revisions(&merge_base, &onto_head)?;
            let onto_paths = committed_paths(&wt, &onto_commits)?;
            overlap = common_paths(&changed, &onto_paths);
            anchors.push(onto_head.sha());
            stopped = matches!(
                wt.operation_in_progress(),
                Some(InProgressOperation::Rebase { .. })
            );
            summary = format!("rebase {} onto {}", ws.name(), onto.name());
        }
        _ => unreachable!(),
    }
    Ok(machine::IntegrationPreview {
        plan: machine::IntegrationPlan {
            effects: match &cli.command {
                Cmd::Merge {
                    source,
                    squash,
                    unstaged,
                    remove,
                    delete_branch,
                    all,
                } => serde_json::json!({
                    "kind":"integration", "operation":"merge", "all":all, "source":ctx.resolve_target(state, source, TargetScope::SameProject)?.name(), "destination":ws.name(),
                    "source_path":ctx.resolve_target(state, source, TargetScope::SameProject)?.path(), "destination_path":ws.path(),
                    "squash":squash.is_some(), "unstaged":unstaged, "remove":remove, "delete_branch":delete_branch,
                    "applied_commits":applied.clone(), "dropped_commits":dropped.clone(), "changed_paths":changed.clone(), "overlap_paths":overlap.clone(),
                    "requested_unstaged_paths":requested.clone(), "untracked_collisions":collisions.clone()
                }),
                Cmd::Rebase { base, all, .. } => serde_json::json!({
                    "kind":"integration", "operation":"rebase", "action":"rebase",
                    "workspace":ws.name(), "path":ws.path(), "base":base, "all":all, "stopped":stopped,
                    "applied_commits":applied.clone(), "dropped_commits":dropped.clone(), "changed_paths":changed.clone(), "overlap_paths":overlap.clone(), "anchors":anchors.clone()
                }),
                _ => unreachable!(),
            },
            warnings: Vec::new(),
            confirmation: format!("Apply the planned {}?", summary),
            applied_commits: applied,
            dropped_commits: dropped,
            anchors,
            changed_paths: changed,
            overlap_paths: overlap,
            requested_unstaged_paths: requested,
            untracked_collisions: collisions,
            stopped,
        },
    })
}

fn machine_action_fields(name: &str) -> &'static [&'static str] {
    match name {
        "default" => &["set"],
        "branchPrefix" | "workspaceBranch" => &["set", "unset"],
        "list" => &["parents"],
        "compare" => &["other"],
        "diff" | "log" => &["from", "to", "rangeMode", "output", "paths"],
        "projectRemove" => &["name", "removeDirectory"],
        "projectRename" => &["name", "newName", "noMove"],
        "projectMove" => &["name", "dir"],
        "projectClone" => &["remote", "dir", "name"],
        "projectInit" => &["path", "name"],
        "add" => &[
            "name", "ref", "branch", "detach", "unstaged", "parent", "parentOf",
        ],
        "branch" => &["name", "ref", "branch", "detach", "unstaged"],
        "remove" => &["target", "keepBranch", "recursive"],
        "rename" => &["target", "newName"],
        "repair" => &["dryRun"],
        "mergePreview" | "merge" => &[
            "mergeSource",
            "squash",
            "all",
            "unstaged",
            "remove",
            "deleteBranch",
        ],
        "rebasePreview" | "rebase" => &["base", "all"],
        "snapshotCreate" => &["archiveRef"],
        "snapshotRestore" => &["snapshotFile", "name"],
        _ => &[],
    }
}

fn machine_action_required(name: &str) -> &'static [&'static str] {
    match name {
        "compare" => &["other"],
        "diff" | "log" => &["from", "to", "rangeMode"],
        "projectRemove" => &["name"],
        "projectRename" => &["name", "newName"],
        "projectMove" => &["name", "dir"],
        "projectClone" => &["remote"],
        "projectInit" => &["path"],
        "add" | "branch" => &["name"],
        "remove" => &["target"],
        "rename" => &["target", "newName"],
        "mergePreview" | "merge" => &["mergeSource"],
        "snapshotRestore" => &["snapshotFile"],
        _ => &[],
    }
}

fn machine_action_constraints(name: &str) -> &'static [&'static str] {
    match name {
        "diff" | "log" => &["typed-range", "repository-relative-paths"],
        "branchPrefix" | "workspaceBranch" => &["set-or-unset"],
        "add" => &[
            "parent-or-parentOf",
            "parentOf-excludes-ref",
            "branch-excludes-detach",
        ],
        "branch" => &["branch-excludes-detach"],
        "merge" | "mergePreview" => &["deleteBranch-requires-remove"],
        _ => &[],
    }
}

fn machine_action_result(name: &str) -> &'static str {
    match name {
        "children" | "siblings" | "list" | "tree" | "snapshotList" => "array",
        "show" => "any",
        _ => "object",
    }
}

fn machine_capabilities() -> serde_json::Value {
    const READS: &[&str] = &[
        "current",
        "path",
        "root",
        "parent",
        "children",
        "siblings",
        "list",
        "show",
        "inspectWorkspace",
        "inspectProject",
        "compare",
        "tree",
        "diff",
        "log",
        "projectCurrent",
        "projectList",
        "snapshotList",
    ];
    const MANAGED_WRITES: &[&str] = &[
        "projectRemove",
        "projectRename",
        "default",
        "branchPrefix",
        "workspaceBranch",
        "repair",
        "projectMove",
        "projectClone",
        "projectInit",
        "add",
        "branch",
        "remove",
        "rename",
    ];
    const INTEGRATIONS: &[&str] = &[
        "mergePreview",
        "merge",
        "rebasePreview",
        "rebase",
        "integrationContinue",
        "integrationAbort",
        "snapshotCreate",
        "snapshotRestore",
    ];
    const COMMANDS: &[&str] = &[];
    let groups = serde_json::json!({
        "reads": READS,
        "managedWrites": MANAGED_WRITES,
        "integrations": INTEGRATIONS,
        "commands": COMMANDS,
    });
    let mut actions = serde_json::Map::new();
    for (group, names) in [
        ("reads", READS),
        ("managedWrites", MANAGED_WRITES),
        ("integrations", INTEGRATIONS),
        ("commands", COMMANDS),
    ] {
        for name in names {
            let effect = if name.ends_with("Preview") || group == "reads" {
                "read"
            } else if matches!(
                *name,
                "default" | "branchPrefix" | "workspaceBranch" | "repair"
            ) {
                "conditional"
            } else {
                "write"
            };
            actions.insert(
                (*name).to_string(),
                serde_json::json!({
                    "description": format!("Run the {name} machine action."),
                    "group": group,
                    "effect": effect,
                    "fields": machine_action_fields(name),
                    "required": machine_action_required(name),
                    "constraints": machine_action_constraints(name),
                    "arguments": [name],
                    "result": machine_action_result(name),
                }),
            );
        }
    }
    serde_json::json!({
        "protocol": "subspace-json",
        "version": 2,
        "features": ["action-contract", "direct-mutation", "mutation-preview"],
        "groups": groups,
        "actions": actions,
    })
}

fn run(cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    if matches!(&cli.command, Cmd::Capabilities) {
        out.success(&machine_capabilities())?;
        return Ok(());
    }
    let registry = Registry::from_env()?;
    let state = registry.read()?;
    if let Some(action) = mutation_action(&cli.command)
        && cli.preview
    {
        if matches!(cli.command, Cmd::Merge { .. } | Cmd::Rebase { .. }) {
            out.success(&integration_preview(&state, cli)?)?;
            return Ok(());
        }
        return cmd_mutation_preview(&state, cli, out, action);
    }
    let global = registry.read_settings()?;
    let settings = cli.settings(io::stdout().is_terminal(), io::stderr().is_terminal());
    let hook_stdout = if cli.json {
        HookStdout::Diagnostic
    } else {
        HookStdout::Inherit
    };
    let hooks = HookRunner::new(global.hooks().clone(), hook_stdout);

    match &cli.command {
        Cmd::Capabilities => unreachable!("capabilities returned before command dispatch"),
        Cmd::Current => cmd_current(&state, out),
        Cmd::Path => cmd_path(&state, cli, out),
        Cmd::Project { action } => {
            cmd_project(&state, cli, &registry, &hooks, settings, out, action)
        }
        Cmd::Root => cmd_root(&state, cli, out),
        Cmd::Parent => cmd_parent(&state, cli, out),
        Cmd::Default { set } => cmd_default(&state, cli, out, set.as_deref()),
        Cmd::BranchPrefix { set, unset } => {
            cmd_branch_prefix(&state, cli, out, set.as_deref(), *unset)
        }
        Cmd::WorkspaceBranch { set, unset } => {
            cmd_workspace_branch(&state, cli, out, set.as_deref(), *unset)
        }
        Cmd::Children => cmd_children(&state, cli, out),
        Cmd::Siblings => cmd_siblings(&state, cli, out),
        Cmd::List { parents } => cmd_list(&state, cli, out, *parents),
        Cmd::Show => cmd_show(&state, cli, out, settings),
        Cmd::Inspect { target } => cmd_inspect(&state, cli, out, target),
        Cmd::Compare { workspace } => cmd_compare(&state, cli, out, workspace),
        Cmd::Tree => cmd_tree(&state, cli, out, settings),
        Cmd::Diff { range, args } => cmd_diff(&state, cli, out, settings, range, args),
        Cmd::Log { range, args } => cmd_log(&state, cli, out, settings, range, args),
        Cmd::Add {
            args,
            parent,
            parent_of,
        } => cmd_add(
            &state,
            cli,
            &hooks,
            out,
            args,
            parent.as_deref(),
            parent_of.as_deref(),
        ),
        Cmd::Branch { args } => cmd_add(&state, cli, &hooks, out, args, Some("@current"), None),
        Cmd::Remove {
            target,
            keep_branch,
            recursive,
        } => cmd_remove(
            &state,
            cli,
            &hooks,
            settings,
            out,
            target,
            *keep_branch,
            *recursive,
        ),
        Cmd::Rename { target, new_name } => cmd_rename(&state, cli, &hooks, out, target, new_name),
        Cmd::Repair { project, dry_run } => {
            cmd_repair(&state, cli, &hooks, out, project.as_deref(), *dry_run)
        }
        Cmd::Merge {
            source,
            squash,
            all,
            unstaged,
            remove,
            delete_branch,
        } => cmd_merge(
            &state,
            cli,
            &hooks,
            settings,
            out,
            source,
            squash.as_deref(),
            *all,
            *unstaged,
            *remove,
            *delete_branch,
        ),
        Cmd::Rebase {
            base,
            interactive,
            all,
        } => cmd_rebase(
            &state,
            cli,
            &hooks,
            settings,
            out,
            base.as_deref(),
            *interactive,
            *all,
        ),
        Cmd::Integration { action } => cmd_integration_recovery(
            &state,
            cli,
            out,
            matches!(action, IntegrationAction::Continue),
        ),
        Cmd::Snapshot { action } => {
            cmd_snapshot(&state, cli, &hooks, settings, out, action.as_ref())
        }
    }
}

fn cmd_current(state: &GlobalState, out: &mut output::Stdio) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let scope = Scope::from_path(state, &cwd)?;
    let scope = scope.ok_or_else(|| anyhow::anyhow!("scope is not a workspace"))?;
    let ws = scope
        .workspace()
        .ok_or_else(|| anyhow::anyhow!("scope is not a workspace"))?;
    let name = ws.full_name();
    if out.json() {
        out.success(&machine::CurrentJson {
            project: ws.project().name(),
            name: ws.name(),
        })?;
    } else {
        out.report(&name);
    }
    Ok(())
}

fn cmd_path(state: &GlobalState, cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let (path, scope) = match &ctx.scope {
        Scope::Workspace(ws) => (ws.path().display().to_string(), "workspace"),
        Scope::Project(p) => (p.path().display().to_string(), "project"),
    };
    if out.json() {
        out.success(&machine::PathJson { path, scope })?;
    } else {
        out.report(&path);
    }
    Ok(())
}

fn cmd_project(
    state: &GlobalState,
    cli: &Cli,
    registry: &Registry,
    hooks: &HookRunner,
    settings: ClientSettings,
    out: &mut output::Stdio,
    action: &ProjectAction,
) -> Result<()> {
    let cmd = &cli.command;
    match action {
        ProjectAction::Current => {
            let cwd = std::env::current_dir()?;
            let scope = Scope::from_path(state, &cwd)?;
            let scope = scope.ok_or_else(|| anyhow::anyhow!("scope is not a project"))?;
            let project = scope.project();
            if out.json() {
                out.success(&machine::ProjectJson {
                    project: project.name(),
                    path: project.path().display().to_string(),
                })?;
            } else {
                out.report(project.name());
            }
        }
        ProjectAction::Remove {
            name,
            remove_directory,
        } => {
            cmd_project_remove(
                state,
                registry,
                hooks,
                settings,
                out,
                name,
                *remove_directory,
            )?;
        }
        ProjectAction::Rename {
            name,
            new_name,
            no_move,
        } => {
            cmd_project_rename(state, registry, hooks, out, cmd, name, new_name, *no_move)?;
        }
        ProjectAction::Move { name, dir } => {
            cmd_project_move(state, registry, hooks, out, cmd, name, dir)?;
        }
        ProjectAction::List => {
            let projects = Project::all(state)?;
            if out.json() {
                let map: BTreeMap<&str, String> = projects
                    .iter()
                    .map(|p| (p.name(), p.path().to_string_lossy().into_owned()))
                    .collect();
                out.success(&map)?;
            } else {
                for project in projects {
                    out.report(project.name());
                }
            }
        }
        ProjectAction::Clone { remote, dir, name } => {
            cmd_project_clone(
                registry,
                hooks,
                out,
                cmd,
                remote,
                dir.as_deref(),
                name.as_deref(),
            )?;
        }
        ProjectAction::Init { path, name } => {
            cmd_project_init(registry, hooks, out, cmd, path, name.as_deref())?;
        }
    }
    Ok(())
}

/// The directory that a new project goes under: `dir` when the caller gives one, and the current
/// directory otherwise.
fn project_parent_dir(dir: Option<&str>) -> Result<PathBuf> {
    match dir {
        Some(d) => {
            let dir = PathBuf::from(d);
            if dir.exists() {
                Ok(dir.canonicalize()?)
            } else if dir.is_absolute() {
                Ok(dir)
            } else {
                Ok(std::env::current_dir()?.join(dir))
            }
        }
        None => Ok(std::env::current_dir()?),
    }
}

fn project_init_name(path: &Path, name: Option<&str>) -> Result<String> {
    if let Some(name) = name {
        return Ok(name.to_string());
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.file_name()
        .and_then(|part| part.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("invalid project path: {}", path.display()))
}

/// Reports a declined prompt as a `declined` failure document, with the given message.
/// Returns whether the caller must stop, which holds whenever `confirmed` is false.
fn report_declined<R: std::io::Write, D: std::io::Write, M: std::io::Write>(
    confirmed: bool,
    message: &str,
    out: &mut output::Channel<R, D, M>,
) -> bool {
    if confirmed {
        return false;
    }
    if out.json() {
        out.failure("declined", message);
    }
    true
}

fn cmd_project_remove<R: io::Write, D: io::Write, M: io::Write>(
    state: &GlobalState,
    registry: &Registry,
    hooks: &HookRunner,
    settings: ClientSettings,
    out: &mut output::Channel<R, D, M>,
    name: &str,
    remove_directory: bool,
) -> Result<()> {
    let path = state
        .get(name)
        .ok_or_else(|| Error::ProjectNotFound(name.to_string()))?
        .to_path_buf();

    let config_dir = paths::project_config_dir(name)?;
    let state_dir = paths::project_state_dir(name)?;
    let prompt = if remove_directory {
        format!(
            "deregister {name} and delete {}, {}, and {}? [y/N] ",
            path.display(),
            config_dir.display(),
            state_dir.display()
        )
    } else {
        format!("deregister {name}? [y/N] ")
    };
    if settings.interactive
        && report_declined(
            confirm(settings, &prompt)?,
            &format!("{name} was not deregistered"),
            out,
        )
    {
        return Ok(());
    }

    let fields = project_fields(&Project::new(name.to_string(), path.clone())?);
    hooks.run(HookKind::ProjectRemove, Phase::Pre, &fields)?;

    registry.update(|state| {
        state.remove(name)?;
        Ok(())
    })?;
    let mut removed = Vec::new();
    if remove_directory {
        std::fs::remove_dir_all(&path)?;
        removed.push(path.display().to_string());
        if config_dir.exists() {
            std::fs::remove_dir_all(&config_dir)?;
            removed.push(config_dir.display().to_string());
        }
        if state_dir.exists() {
            std::fs::remove_dir_all(&state_dir)?;
            removed.push(state_dir.display().to_string());
        }
    }
    out.note(&format!("deregistered {name}"));
    hooks.run(HookKind::ProjectRemove, Phase::Post, &fields)?;
    if out.json() {
        out.success(&machine::ProjectRemovedJson {
            project: name,
            path: path.display().to_string(),
            removed,
        })?;
    }
    Ok(())
}

/// Rewrites the worktree links that a move of the project directory breaks. Every linked worktree
/// holds an absolute path back into the repository, so the move breaks all of them until git
/// rewrites the links. A project whose repository does not open has no repair, and the notice says
/// so.
fn repair_worktree_links(
    project: &Project,
    lock: &ProjectLock,
    out: &mut output::Stdio,
) -> Result<()> {
    match project.anchor_worktree() {
        Ok(anchor) => {
            let paths: Vec<PathBuf> = project.workspaces()?.iter().map(|ws| ws.path()).collect();
            Ok(anchor.repair_paths(lock, &paths)?)
        }
        Err(e) => {
            out.note(&format!("cannot repair worktree links: {e}"));
            Ok(())
        }
    }
}

/// Resolves a registered project by name, for a change to its directory. The directory must be a
/// project root.
fn registered_project(state: &GlobalState, name: &str) -> Result<Project> {
    let path = state
        .get(name)
        .ok_or_else(|| Error::ProjectNotFound(name.to_string()))?
        .to_path_buf();
    Project::open(name.to_string(), path.clone())?
        .ok_or_else(|| anyhow::anyhow!("not a project root: {}", path.display()))
}

/// Returns the original failure after restoring the name-keyed files and project directory.
fn rollback_relocation(
    project: &Project,
    new_path: &Path,
    moved: bool,
    undo: impl FnOnce() -> Result<()>,
    error: anyhow::Error,
) -> anyhow::Error {
    let mut failures = Vec::new();
    if let Err(failure) = undo() {
        failures.push(format!("name-keyed files: {failure:#}"));
    }
    if moved && let Err(failure) = std::fs::rename(new_path, project.path()) {
        failures.push(format!("project directory: {failure}"));
    }
    if failures.is_empty() {
        error
    } else {
        anyhow::anyhow!("{error:#}\nrollback failed:\n{}", failures.join("\n"))
    }
}

/// Moves `project` to `new_name` at `new_path`, and points everything that names it at the new
/// place. It moves the directory, runs `finish` for the name-keyed moves of the caller, and
/// rewrites the registry last. So a failure leaves the old name everywhere, or a project that
/// resolves in whole.
///
/// It repairs the worktree links that the directory move broke last of all, after the registry
/// write. So a repair that fails leaves a project that still resolves, and `subspace repair` can
/// finish the work.
///
/// The caller reports the outcome after the project lock drops.
#[allow(clippy::too_many_arguments)]
fn relocate_project<U>(
    registry: &Registry,
    out: &mut output::Stdio,
    cmd: &Cmd,
    project: &Project,
    new_name: &str,
    new_path: &Path,
    finish: impl FnOnce(&Project) -> Result<U>,
) -> Result<()>
where
    U: FnOnce() -> Result<()>,
{
    let holder = lock_holder(&Scope::Project(project.clone()), cmd);
    let lock = project.lock(&holder)?;
    let target = Project::new(new_name.to_string(), new_path.to_path_buf())?;
    let moved = new_path != project.path();
    if moved {
        std::fs::rename(project.path(), new_path)?;
    }

    let undo = match finish(&target) {
        Ok(undo) => undo,
        Err(error) => {
            return Err(rollback_relocation(
                project,
                new_path,
                moved,
                || Ok(()),
                error,
            ));
        }
    };

    if let Err(error) = registry.update(|state| {
        state.remove(project.name())?;
        state.insert(new_name, new_path.to_path_buf())
    }) {
        return Err(rollback_relocation(
            project,
            new_path,
            moved,
            undo,
            error.into(),
        ));
    }

    if moved {
        repair_worktree_links(&target, &lock, out)?;
    }
    drop(lock);

    Ok(())
}

fn remove_new_empty_config_dir(path: Option<&Path>, existed: bool) -> Result<()> {
    if !existed
        && let Some(path) = path
        && path.is_dir()
        && std::fs::read_dir(path)?.next().is_none()
    {
        std::fs::remove_dir(path)?;
    }
    Ok(())
}

fn relocate_project_metadata(
    old_config: &Path,
    new_config: &Path,
    old_state: &Path,
    new_state: &Path,
) -> Result<Box<dyn FnOnce() -> Result<()>>> {
    let old_config = old_config.to_path_buf();
    let new_config = new_config.to_path_buf();
    let old_state = old_state.to_path_buf();
    let new_state = new_state.to_path_buf();
    let moved_config = old_config.exists();
    let moved_state = old_state.exists();
    let target_config_dir = new_config.parent().map(Path::to_path_buf);
    let target_config_dir_existed = target_config_dir.as_ref().is_some_and(|path| path.exists());
    if moved_config && let Some(parent) = &target_config_dir {
        std::fs::create_dir_all(parent)?;
    }
    if moved_state && let Some(parent) = new_state.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if moved_config {
        std::fs::rename(&old_config, &new_config)?;
    }
    if moved_state && let Err(error) = std::fs::rename(&old_state, &new_state) {
        if moved_config {
            std::fs::rename(&new_config, &old_config).map_err(|rollback| {
                anyhow::anyhow!("state move failed: {error}\nconfig rollback failed: {rollback}")
            })?;
            remove_new_empty_config_dir(target_config_dir.as_deref(), target_config_dir_existed)?;
        }
        return Err(error.into());
    }
    Ok(Box::new(move || {
        if moved_state {
            std::fs::rename(&new_state, &old_state)?;
        }
        if moved_config {
            std::fs::rename(&new_config, &old_config)?;
            remove_new_empty_config_dir(target_config_dir.as_deref(), target_config_dir_existed)?;
        }
        Ok(())
    }))
}

#[allow(clippy::too_many_arguments)]
fn cmd_project_rename(
    state: &GlobalState,
    registry: &Registry,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    cmd: &Cmd,
    name: &str,
    new_name: &str,
    no_move: bool,
) -> Result<()> {
    let project = registered_project(state, name)?;

    validate_project_name(new_name)?;
    if new_name == project.name() {
        bail!("project is already named {new_name}");
    }
    if state.get(new_name).is_some() {
        return Err(Error::ProjectExists(new_name.to_string()).into());
    }

    let new_metadata_config =
        Project::new(new_name.to_string(), project.path().to_path_buf())?.metadata_config_path();
    if new_metadata_config.exists() {
        bail!(
            "config file already exists: {}",
            new_metadata_config.display()
        );
    }
    let new_state = paths::project_state_dir(new_name)?;
    if new_state.exists() {
        bail!("state directory already exists: {}", new_state.display());
    }

    let new_path = if no_move {
        project.path().to_path_buf()
    } else {
        let path = rename_target_path(&project, new_name)?;
        if path.exists() {
            bail!("directory already exists: {}", path.display());
        }
        path
    };

    let moved = new_path != project.path();
    let old_path = project.path().to_path_buf();
    let old_config = project.metadata_config_path();
    let old_state = project.state_path();

    let renamed = Project::new(new_name.to_string(), new_path.clone())?;
    let fields = project_fields(&renamed)
        .previous(project.name())
        .previous_path(old_path.to_string_lossy());
    hooks.run(HookKind::ProjectRename, Phase::Pre, &fields)?;

    relocate_project(
        registry,
        out,
        cmd,
        &project,
        new_name,
        &new_path,
        |renamed| {
            relocate_project_metadata(
                &old_config,
                &renamed.metadata_config_path(),
                &old_state,
                &renamed.state_path(),
            )
        },
    )?;

    out.note(&format!("renamed {} -> {new_name}", project.name()));
    if moved {
        out.note(&format!("moved to {}", new_path.display()));
    }

    hooks.run(HookKind::ProjectRename, Phase::Post, &fields)?;

    if out.json() {
        out.success(&machine::ProjectRenameJson {
            project: new_name,
            previous: project.name(),
            path: new_path.display().to_string(),
            previous_path: old_path.display().to_string(),
        })?;
    }
    Ok(())
}

fn cmd_project_move(
    state: &GlobalState,
    registry: &Registry,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    cmd: &Cmd,
    name: &str,
    dir: &str,
) -> Result<()> {
    let project = registered_project(state, name)?;

    let dest = PathBuf::from(dir);
    let dest = dest
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("directory does not exist: {}", dest.display()))?;
    let new_path = move_target_path(&project, &dest)?;
    if new_path == project.path() {
        bail!("project is already in {}", dest.display());
    }
    if new_path.exists() {
        bail!("directory already exists: {}", new_path.display());
    }

    if std::fs::metadata(project.path())?.dev() != std::fs::metadata(&dest)?.dev() {
        bail!(
            "cannot move across filesystems: {} and {} are on different devices",
            project.path().display(),
            dest.display()
        );
    }

    let old_path = project.path().to_path_buf();

    let moved_project = Project::new(name.to_string(), new_path.clone())?;
    let fields = project_fields(&moved_project).previous_path(old_path.to_string_lossy());
    hooks.run(HookKind::ProjectMove, Phase::Pre, &fields)?;

    relocate_project(registry, out, cmd, &project, name, &new_path, |_| {
        Ok(|| Ok(()))
    })?;

    out.note(&format!("moved {name} to {}", new_path.display()));

    hooks.run(HookKind::ProjectMove, Phase::Post, &fields)?;

    if out.json() {
        out.success(&machine::ProjectMoveJson {
            project: name,
            path: new_path.display().to_string(),
            previous_path: old_path.display().to_string(),
        })?;
    }
    Ok(())
}

fn rollback_new_project(project: &Project, error: anyhow::Error) -> anyhow::Error {
    let mut failures = Vec::new();
    for path in [project.path().to_path_buf(), project.state_path()] {
        if let Err(cause) = std::fs::remove_dir_all(&path) {
            failures.push(format!("{}: {cause}", path.display()));
        }
    }
    if failures.is_empty() {
        error
    } else {
        error.context(format!("project cleanup failed: {}", failures.join(", ")))
    }
}

fn finish_new_project(
    registry: &Registry,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    project: &Project,
    lock: ProjectLock,
    workspace: &Workspace,
) -> Result<()> {
    let registration = (|| {
        let branch = workspace.name().to_string();
        project.metadata().update(&lock, |meta| {
            meta.workspaces
                .entry(branch.clone())
                .or_insert_with(WorkspaceMeta::default)
                .branch = Some(branch.clone());
            Ok(())
        })?;

        let canonical_path = project.path().canonicalize()?;
        registry.update(|state| state.insert(project.name(), canonical_path.clone()))?;
        Ok::<_, anyhow::Error>(canonical_path)
    })();
    let canonical_path = match registration {
        Ok(path) => path,
        Err(error) => {
            drop(lock);
            return Err(rollback_new_project(project, error));
        }
    };

    drop(lock);

    run_post_hooks(
        hooks,
        &[
            (HookKind::ProjectAdd, project_fields(project)),
            (HookKind::WorkspaceAdd, workspace_fields(workspace)),
        ],
    )?;

    if out.json() {
        out.success(&machine::ProjectCreatedJson {
            project: project.name(),
            path: canonical_path.display().to_string(),
            workspace: workspace.name(),
        })?;
    }
    Ok(())
}

/// The project name that a remote URL yields: the last path segment without any `.git` suffix.
fn derive_clone_name(remote: &str) -> &str {
    let last = remote
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(remote);
    last.strip_suffix(".git").unwrap_or(last)
}

fn cmd_project_clone(
    registry: &Registry,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    cmd: &Cmd,
    remote: &str,
    dir: Option<&str>,
    name: Option<&str>,
) -> Result<()> {
    let name = name.unwrap_or_else(|| derive_clone_name(remote));
    validate_project_name(name)?;
    if registry.read()?.get(name).is_some() {
        return Err(Error::ProjectExists(name.to_string()).into());
    }

    let remote = if Path::new(remote).exists() {
        std::fs::canonicalize(remote)?
            .to_string_lossy()
            .into_owned()
    } else {
        remote.to_string()
    };
    let path = project_parent_dir(dir)?.join(name);
    let planned = Project::new(name.to_string(), path.clone())?;
    hooks.run(HookKind::ProjectAdd, Phase::Pre, &project_fields(&planned))?;
    let holder = lock_holder(&Scope::Project(planned), cmd);
    let (project, lock, workspace) =
        create_project(name.to_string(), &path, &holder, |worktrees, _lock| {
            let worktree = worktree::clone_repo(worktrees, &remote, &worktrees.join(".clone-tmp"))?;
            let branch = worktree.get_branch().unwrap_or_else(|| "main".to_string());
            validate_name(&branch)?;
            worktree::rename_worktree(worktree, &worktrees.join(&branch))?;
            Ok(branch)
        })?;

    finish_new_project(registry, hooks, out, &project, lock, &workspace)
}

fn cmd_project_init(
    registry: &Registry,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    cmd: &Cmd,
    path: &str,
    name: Option<&str>,
) -> Result<()> {
    let path = PathBuf::from(path);
    let name = project_init_name(&path, name)?;
    validate_project_name(&name)?;
    if registry.read()?.get(&name).is_some() {
        return Err(Error::ProjectExists(name).into());
    }
    let planned = Project::new(name.clone(), path.clone())?;
    hooks.run(HookKind::ProjectAdd, Phase::Pre, &project_fields(&planned))?;
    let holder = lock_holder(&Scope::Project(planned), cmd);
    let (project, lock, workspace) =
        create_project(name.to_string(), &path, &holder, |worktrees, _lock| {
            let worktree = Worktree::init_empty(&worktrees.join(".init-tmp"))?;
            let branch = worktree
                .head_branch_name()
                .unwrap_or_else(|| "main".to_string());
            validate_name(&branch)?;
            worktree::rename_worktree(worktree, &worktrees.join(&branch))?;
            Ok(branch)
        })?;

    finish_new_project(registry, hooks, out, &project, lock, &workspace)
}

fn cmd_root(state: &GlobalState, cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let meta = ws.project().metadata().read()?;
    let root = ws.root_workspace(&meta)?;
    if out.json() {
        out.success(&machine::NameJson { name: root.name() })?;
    } else {
        out.report(root.name());
    }
    Ok(())
}

fn cmd_parent(state: &GlobalState, cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let meta = ws.project().metadata().read()?;
    let parent = ws
        .parent_workspace(&meta)
        .ok_or_else(|| anyhow::anyhow!("{} has no parent workspace", ws.name()))?;
    if out.json() {
        out.success(&machine::NameJson {
            name: parent.name(),
        })?;
    } else {
        out.report(parent.name());
    }
    Ok(())
}

fn cmd_default(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    workspace: Option<&str>,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let project = ctx.scope.project();
    let value = match workspace {
        None => {
            let ws = project.default_workspace()?.ok_or_else(|| {
                anyhow::anyhow!("no default workspace configured for {}", project.name())
            })?;
            let name = ws.name().to_string();
            if !out.json() {
                out.report(&name);
            }
            name
        }
        Some(name) => {
            if Workspace::get(project, name).is_none() {
                bail!("workspace not found: {name}");
            }
            let lock = project.lock(&lock_holder(&ctx.scope, &cli.command))?;
            project.metadata().update(&lock, |meta| {
                meta.default = Some(name.to_string());
                Ok(())
            })?;
            out.note(&format!("default workspace set to {name}"));
            name.to_string()
        }
    };
    if out.json() {
        out.success(&machine::SettingJson {
            project: project.name(),
            setting: "default",
            value: Some(&value),
        })?;
    }
    Ok(())
}

fn cmd_branch_prefix(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    value: Option<&str>,
    unset: bool,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let project = ctx.scope.project();
    if !unset && value.is_none() {
        let prefix = project.branch_prefix()?;
        if out.json() {
            out.success(&machine::SettingJson {
                project: project.name(),
                setting: "branch_prefix",
                value: prefix.as_deref(),
            })?;
        } else if let Some(prefix) = &prefix {
            out.report(prefix);
        }
        return Ok(());
    }
    let lock = project.lock(&lock_holder(&ctx.scope, &cli.command))?;
    project.metadata().update(&lock, |meta| {
        meta.branch_prefix = if unset {
            None
        } else {
            value.map(str::to_string)
        };
        Ok(())
    })?;
    match value {
        Some(prefix) => out.note(&format!("branch prefix set to {prefix}")),
        None => out.note("branch prefix cleared"),
    }
    if out.json() {
        out.success(&machine::SettingJson {
            project: project.name(),
            setting: "branch_prefix",
            value,
        })?;
    }
    Ok(())
}

fn cmd_workspace_branch(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Channel<impl Write, impl Write, impl Write>,
    value: Option<&str>,
    unset: bool,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    workspace_branch(ws, cli, out, value, unset)
}

fn workspace_branch(
    ws: &Workspace,
    cli: &Cli,
    out: &mut output::Channel<impl Write, impl Write, impl Write>,
    value: Option<&str>,
    unset: bool,
) -> Result<()> {
    let project = ws.project();
    let branch = if value.is_some() || unset {
        let lock = project.lock(&lock_holder(&Scope::Workspace(ws.clone()), &cli.command))?;
        project.metadata().update(&lock, |meta| {
            if let Some(branch) = value {
                meta.workspaces
                    .entry(ws.name().to_string())
                    .or_default()
                    .branch = Some(branch.to_string());
            } else if let Some(entry) = meta.workspaces.get_mut(ws.name()) {
                entry.branch = None;
            }
            Ok(())
        })?;
        match value {
            Some(branch) => out.note(&format!("workspace branch set to {branch}")),
            None => out.note("workspace branch cleared"),
        }
        value.map(str::to_string)
    } else {
        project
            .metadata()
            .read()?
            .workspaces
            .get(ws.name())
            .and_then(|entry| entry.branch.clone())
    };
    if out.json() {
        out.success(&machine::WorkspaceSettingJson {
            project: project.name(),
            workspace: ws.name(),
            setting: "workspace_branch",
            value: branch.as_deref(),
        })?;
    } else if value.is_none()
        && !unset
        && let Some(branch) = branch
    {
        out.report(&branch);
    }
    Ok(())
}

fn cmd_children(state: &GlobalState, cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let meta = ws.project().metadata().read()?;
    let children = ws.children(&meta)?;
    if out.json() {
        let names: Vec<&str> = children.iter().map(|ws| ws.name()).collect();
        out.success(&names)?;
    } else {
        for child in &children {
            out.report(child.name());
        }
    }
    Ok(())
}

fn cmd_siblings(state: &GlobalState, cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let meta = ws.project().metadata().read()?;
    let siblings = ws.siblings(&meta)?;
    if out.json() {
        let names: Vec<&str> = siblings.iter().map(|ws| ws.name()).collect();
        out.success(&names)?;
    } else {
        for sibling in &siblings {
            out.report(sibling.name());
        }
    }
    Ok(())
}

/// Prints the workspace names of the project. The machine result always names the parent edge.
/// `--parents` governs the text form alone: it adds a `name<TAB>parent` shape there, so one action
/// keeps one machine shape.
fn cmd_list(state: &GlobalState, cli: &Cli, out: &mut output::Stdio, parents: bool) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let workspaces = ctx.scope.project().workspaces()?;
    let edges = if out.json() || parents {
        let meta = ctx.scope.project().metadata().read()?;
        let known: HashSet<&str> = workspaces.iter().map(|ws| ws.name()).collect();
        Some(meta.edges(&known))
    } else {
        None
    };
    if out.json() {
        let names: Vec<&str> = workspaces.iter().map(|ws| ws.name()).collect();
        out.success(&machine::list_entries_json(&names, edges.as_ref().unwrap()))?;
        return Ok(());
    }
    if let Some(edges) = &edges {
        for ws in &workspaces {
            let parent = edges.get(ws.name()).map(String::as_str).unwrap_or("");
            out.report(&format!("{}\t{}", ws.name(), parent));
        }
    } else {
        for ws in &workspaces {
            out.report(ws.name());
        }
    }
    Ok(())
}

fn cmd_show(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    settings: ClientSettings,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let color = settings.color;
    let meta = ctx.scope.project().metadata().read()?;

    match ctx.scope.workspace() {
        Some(workspace) => {
            let info = workspace.info(&meta)?;
            if out.json() {
                out.success(&machine::workspace_json(&info, &meta, workspace.name()))?;
            } else {
                println!(
                    "{}",
                    format::format_workspace_details(
                        workspace.name(),
                        &workspace_label(&info),
                        color
                    )
                );
            }
        }
        None => {
            let trees = WorkspaceTree::roots(ctx.scope.project())?;
            let mut info_map = HashMap::new();
            for tree in &trees {
                for node in tree.nodes() {
                    if let Ok(info) = node.info(&meta) {
                        info_map.insert(node.name().to_string(), info);
                    }
                }
            }
            let mut workspaces = ctx.scope.project().workspaces()?;
            workspaces.sort_by(|a, b| a.name().cmp(b.name()));
            if out.json() {
                let views: Vec<_> = workspaces
                    .iter()
                    .map(|ws| machine::StatusEntryJson {
                        name: ws.name(),
                        status: info_map
                            .get(ws.name())
                            .map(|info| machine::workspace_json(info, &meta, ws.name())),
                    })
                    .collect();
                out.success(&views)?;
            } else {
                for ws in &workspaces {
                    match info_map.get(ws.name()) {
                        Some(info) => println!(
                            "{}",
                            format::format_workspace_details(
                                ws.name(),
                                &workspace_label(info),
                                color
                            )
                        ),
                        None => println!("{}", ws.name()),
                    }
                }
            }
        }
    }
    Ok(())
}

fn cmd_inspect<R: Write, D: Write, M: Write>(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Channel<R, D, M>,
    target: &InspectTarget,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    match target {
        InspectTarget::Workspace => {
            let ws = ctx.scope.require_workspace()?;
            let meta = ws.project().metadata().read()?;
            let info = ws.info(&meta)?;
            let root = ws.root_workspace(&meta)?;
            let children = ws
                .children(&meta)?
                .into_iter()
                .map(|w| w.name().to_string())
                .collect();
            let head = ws.worktree()?.head()?;
            let status = machine::workspace_json(&info, &meta, ws.name());
            if out.json() {
                out.success(&machine::WorkspaceInspection {
                    project: ws.project().name().to_string(),
                    name: ws.name().to_string(),
                    path: ws.path().display().to_string(),
                    branch: status.branch.map(str::to_string),
                    head: head.sha(),
                    parent: status.parent.as_ref().map(|p| p.name.clone()),
                    root: root.name().to_string(),
                    children,
                    state_path: ws.state_path().display().to_string(),
                    status: machine::WorkspaceJsonOwned {
                        branch: status.branch.map(str::to_string),
                        parent: status.parent,
                        remote: status.remote,
                        operation: status.operation,
                        files: status.files,
                    },
                })?;
            } else {
                out.report(&format!(
                    "{}/{} {}",
                    ws.project().name(),
                    ws.name(),
                    ws.path().display()
                ));
            }
        }
        InspectTarget::Project => {
            let project = ctx.scope.project();
            let meta = project.metadata().read()?;
            let workspaces = project.workspaces()?;
            let known: HashSet<&str> = workspaces.iter().map(|w| w.name()).collect();
            let edges = meta.edges(&known);
            let plan = subspace::workspace::plan_repair(project, &meta)?;
            if out.json() {
                out.success(&machine::ProjectInspection {
                    project: project.name().to_string(),
                    path: project.path().display().to_string(),
                    config_path: project.config_path().display().to_string(),
                    state_path: project.state_path().display().to_string(),
                    metadata_config_path: project.metadata_config_path().display().to_string(),
                    metadata_state_path: project.metadata_state_path().display().to_string(),
                    default: meta.default,
                    branch_prefix: meta.branch_prefix,
                    workspaces: workspaces
                        .iter()
                        .map(|ws| machine::ListEntryOwned {
                            name: ws.name().to_string(),
                            parent: edges.get(ws.name()).cloned(),
                        })
                        .collect(),
                    configuration: machine::ConfigurationState {
                        config_exists: project.config_path().exists(),
                        state_exists: project.state_path().exists(),
                    },
                    repair: machine::RepairState {
                        needed: !plan.is_empty(),
                        pruned: plan.pruned().to_vec(),
                        reparented: plan
                            .reparented()
                            .map(|(workspace, parent)| machine::ReparentOwned {
                                workspace: workspace.to_string(),
                                parent: parent.map(str::to_string),
                            })
                            .collect(),
                    },
                })?;
            } else {
                out.report(project.name());
            }
        }
    }
    Ok(())
}

fn cmd_compare<R: Write, D: Write, M: Write>(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Channel<R, D, M>,
    target: &str,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let current = ctx.scope.require_workspace()?;
    let other = ctx.resolve_target(state, target, TargetScope::Any)?;
    let current_wt = current.worktree()?;
    let other_wt = other.worktree()?;
    let current_head = current_wt.head()?;
    let other_head = other_wt.head()?;
    let current_only = other_wt.commits_only_in(&current_head, &other_head)?;
    let workspace_only = other_wt.commits_only_in(&other_head, &current_head)?;
    let range = format!("{}...{}", current_head.sha(), other_head.sha());
    let diff = worktree::git_command()
        .arg("-C")
        .arg(other.path())
        .args(["diff", "--stat", &range])
        .output()?;
    let names = worktree::git_command()
        .arg("-C")
        .arg(other.path())
        .args(["diff", "--name-status", &range])
        .output()?;
    let paths = String::from_utf8_lossy(&names.stdout)
        .lines()
        .filter_map(|line| {
            let (status, path) = line.split_once('\t')?;
            Some(machine::ComparisonPath {
                status: status.to_string(),
                path: path.to_string(),
            })
        })
        .collect();
    let dirty_paths = other_wt
        .status()?
        .index
        .into_iter()
        .chain(other_wt.status()?.worktree)
        .map(|(path, _)| path.display().to_string())
        .collect();
    let current_meta = current.project().metadata().read()?;
    let rewritten = current
        .info(&current_meta)?
        .parent
        .is_some_and(|p| p.name == other.name() && p.rewritten);
    if out.json() {
        out.success(&machine::WorkspaceComparison {
            project: current.project().name().to_string(),
            current: current.name().to_string(),
            workspace: other.name().to_string(),
            current_head: current_head.sha(),
            workspace_head: other_head.sha(),
            commits: machine::ComparisonCommits {
                current_only,
                workspace_only,
            },
            statistics: String::from_utf8_lossy(&diff.stdout).trim().to_string(),
            paths,
            dirty_paths,
            rewritten,
        })?;
    } else {
        out.report(&format!("{} vs {}", current.name(), other.name()));
    }
    Ok(())
}

fn workspace_label(info: &WorkspaceInfo) -> format::WorkspaceLabel<'_> {
    format::WorkspaceLabel {
        git_status: &info.git_status,
        parent: info.parent.as_ref().map(|p| format::ParentInfo {
            commits_behind: p.commits_behind,
            commits_ahead: p.commits_ahead,
        }),
        operation: info.operation,
    }
}

fn cmd_tree(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    settings: ClientSettings,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let project = ctx.scope.project();
    let color = settings.color;
    let meta = project.metadata().read()?;

    let sort_key = |ws: &Workspace| ws.name().to_string();

    let mark_name = ctx.scope.workspace().map(|ws| ws.name().to_string());

    let trees = match ctx.scope.workspace() {
        Some(ws) => vec![WorkspaceTree::from_workspace(ws)?],
        None => WorkspaceTree::roots(project)?,
    };
    if out.json() {
        let mut info_map = HashMap::new();
        for tree in &trees {
            for node in tree.nodes() {
                if let Ok(info) = node.info(&meta) {
                    info_map.insert(node.name().to_string(), info);
                }
            }
        }
        out.success(&machine::tree_nodes_json(
            &trees,
            &info_map,
            &meta,
            mark_name.as_deref(),
        ))?;
        return Ok(());
    }
    for (i, tree) in trees.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let mut info_map = HashMap::new();
        for node in tree.nodes() {
            if let Ok(info) = node.info(&meta) {
                info_map.insert(node.name().to_string(), info);
            }
        }
        let label = |ws: &Workspace| -> String {
            let display_name = if mark_name.as_deref() == Some(ws.name()) {
                format::ansi(format::Color::Cyan, ws.name(), color)
            } else {
                ws.name().to_string()
            };
            match info_map.get(ws.name()) {
                Some(info) => {
                    format::format_tree_workspace(&display_name, &workspace_label(info), color)
                }
                None => display_name,
            }
        };
        print!("{}", format::render_tree(tree, &sort_key, &label));
    }
    Ok(())
}

fn resolve_endpoint(ctx: &Context, state: &GlobalState, key: &str) -> Result<String> {
    let key = if key.is_empty() { "@current" } else { key };
    let ws = ctx.resolve_target(state, key, TargetScope::SameProject)?;
    Ok(ws.worktree()?.head()?.sha())
}

fn parse_range(ctx: &Context, state: &GlobalState, target: &str) -> Result<String> {
    if let Some((left, right)) = target.split_once("...") {
        let left = resolve_endpoint(ctx, state, left)?;
        let right = resolve_endpoint(ctx, state, right)?;
        Ok(format!("{left}...{right}"))
    } else if let Some((left, right)) = target.split_once("..") {
        let left = resolve_endpoint(ctx, state, left)?;
        let right = resolve_endpoint(ctx, state, right)?;
        Ok(format!("{left}..{right}"))
    } else {
        let left = resolve_endpoint(ctx, state, target)?;
        let right = resolve_endpoint(ctx, state, "@current")?;
        Ok(format!("{left}...{right}"))
    }
}

fn format_range(left: String, right: String) -> String {
    format!("{left}...{right}")
}

fn flag_range(ctx: &Context, state: &GlobalState, range: &RangeArgs) -> Result<Option<String>> {
    if let (Some(left), Some(right)) = (range.from.as_deref(), range.to.as_deref()) {
        let left = resolve_endpoint(ctx, state, left)?;
        let right = resolve_endpoint(ctx, state, right)?;
        return Ok(Some(format_range(left, right)));
    }
    if range.parent {
        let left = resolve_endpoint(ctx, state, "@parent")?;
        let right = resolve_endpoint(ctx, state, "@current")?;
        return Ok(Some(format_range(left, right)));
    }
    if range.root {
        let left = resolve_endpoint(ctx, state, "@root")?;
        let right = resolve_endpoint(ctx, state, "@current")?;
        return Ok(Some(format_range(left, right)));
    }
    if let Some(ws) = range.in_ws.as_deref() {
        let left = resolve_endpoint(ctx, state, "@parent")?;
        let right = resolve_endpoint(ctx, state, ws)?;
        return Ok(Some(format_range(left, right)));
    }
    if let Some(ws) = range.from.as_deref() {
        let left = resolve_endpoint(ctx, state, ws)?;
        let right = resolve_endpoint(ctx, state, "@current")?;
        return Ok(Some(format_range(left, right)));
    }
    if let Some(ws) = range.to.as_deref() {
        let left = resolve_endpoint(ctx, state, "@current")?;
        let right = resolve_endpoint(ctx, state, ws)?;
        return Ok(Some(format_range(left, right)));
    }
    Ok(None)
}

fn split_target<'a>(
    ctx: &Context,
    state: &GlobalState,
    range: &RangeArgs,
    args: &'a [String],
) -> Result<(Option<String>, &'a [String])> {
    let positional = match args.split_first() {
        Some((first, rest)) if !first.starts_with('-') => Some((first.as_str(), rest)),
        _ => None,
    };
    match (flag_range(ctx, state, range)?, positional) {
        (Some(_), Some((first, _))) => {
            bail!("range flag conflicts with positional target: {first}")
        }
        (Some(rev), None) => Ok((Some(rev), args)),
        (None, Some((first, rest))) => Ok((Some(parse_range(ctx, state, first)?), rest)),
        (None, None) => Ok((None, args)),
    }
}

/// The `--color=<when>` argument of git, from the resolved color setting. It goes at the front of a
/// git invocation that passes arguments through, so a `--color` value that only git knows, such as
/// `--color=true`, still wins from the trailing arguments.
fn git_color_arg(settings: ClientSettings) -> String {
    let when = if settings.color { "always" } else { "never" };
    format!("--color={when}")
}

/// The [`ColorChoice`] that a `--color` value names, or `None` for a value that only git knows,
/// such as `true`.
fn color_choice(value: &str) -> Option<ColorChoice> {
    ColorChoice::from_str(value, false).ok()
}

/// Takes a `--color` out of the arguments that `diff` and `log` pass to git, and folds it into the
/// settings.
///
/// Clap gives the trailing arguments every token after the first one it does not know. So a
/// `--color` after such a token reaches git, which reads the value of the space form as a revision.
/// This handles the `--color never` form and the `--color=never` form instead, and the last one
/// wins. It leaves a value that only git knows in place for git.
///
/// In JSON mode, `auto` means no color, because the machine result is not a terminal.
fn take_passthrough_color(
    settings: ClientSettings,
    json: bool,
    args: &[String],
) -> (ClientSettings, Vec<String>) {
    let mut choice = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let taken = match arg.strip_prefix("--color=") {
            Some(value) => color_choice(value),
            None if arg == "--color" => {
                let value = args.peek().and_then(|next| color_choice(next));
                if value.is_some() {
                    args.next();
                }
                value
            }
            None => None,
        };
        match taken {
            Some(value) => choice = Some(value),
            None => rest.push(arg.clone()),
        }
    }
    let settings = choice.map_or(settings, |choice| {
        let flag = match (choice, json) {
            (ColorChoice::Auto, true) => Some(false),
            (choice, _) => color_flag(choice),
        };
        settings.with_color(flag)
    });
    (settings, rest)
}

/// Warns when an edge on the chain of `ws` up to `bound` rewrote the history below its anchor. It
/// does not fail. A `bound` of `None` scans up to the root.
///
/// An error of the scan, such as a dangling parent, becomes a warning too, so a read-only command
/// like `diff` or `log` always runs.
fn warn_if_parent_rewritten(
    ws: &Workspace,
    meta: &ProjectMeta,
    bound: Option<&Workspace>,
    out: &mut output::Stdio,
) {
    match ws.first_rewritten_edge(meta, bound) {
        Ok(Some((child, dropped))) => {
            let message = Error::ParentRewritten {
                workspace: child.name().to_string(),
                dropped,
            };
            out.warn(&message.to_string());
        }
        Ok(None) => {}
        Err(err) => out.warn(&err.to_string()),
    }
}

/// The rewrite scan that a range flag asks for: no scan, or a scan that starts at one workspace and
/// stops at an ancestor. A `bound` of `None` walks to the root.
enum RewriteScan {
    NoScan,
    Scan {
        start: Workspace,
        bound: Option<Workspace>,
    },
}

/// Resolves the rewrite scan that a range asks for.
///
/// `--parent` scans `ws` and stops at its direct parent. `--root` scans `ws` and walks to the root.
/// `--in <WS>` scans the resolved `WS`, which is the right endpoint of the range, because `--in`
/// runs from `@parent` to `WS`, and stops at the direct parent of `ws`. `--from <WS>` and `--to
/// <WS>` scan `ws` and stop at the resolved `WS`, when `WS` is an ancestor of `ws` in the stored
/// parent chain. The scan follows the stored ancestor relation, not the direction of the diff.
///
/// It gives `NoScan` when no range flag is set, when the target of `--in`, `--from`, or `--to` does
/// not resolve, and when the target of `--from` or `--to` is not an ancestor of `ws`.
fn range_rewrite_scan(
    ctx: &Context,
    state: &GlobalState,
    ws: &Workspace,
    range: &RangeArgs,
    meta: &ProjectMeta,
    out: &mut output::Stdio,
) -> Result<RewriteScan> {
    if range.parent {
        return Ok(RewriteScan::Scan {
            start: ws.clone(),
            bound: ws.parent_workspace(meta),
        });
    }
    if range.root {
        return Ok(RewriteScan::Scan {
            start: ws.clone(),
            bound: None,
        });
    }
    if let Some(target) = range.in_ws.as_deref() {
        return Ok(
            match ctx.resolve_target(state, target, TargetScope::SameProject) {
                Ok(target_ws) => RewriteScan::Scan {
                    start: target_ws,
                    bound: ws.parent_workspace(meta),
                },
                Err(_) => RewriteScan::NoScan,
            },
        );
    }
    if let Some(target) = range.from.as_deref().or(range.to.as_deref()) {
        return ancestor_rewrite_scan(ctx, state, ws, target, meta, out);
    }
    Ok(RewriteScan::NoScan)
}

/// Resolves `target`. When the target is an ancestor of `ws` in the stored parent chain, it scans
/// `ws` and stops at the target. `--from` and `--to` share it.
fn ancestor_rewrite_scan(
    ctx: &Context,
    state: &GlobalState,
    ws: &Workspace,
    target: &str,
    meta: &ProjectMeta,
    out: &mut output::Stdio,
) -> Result<RewriteScan> {
    let target_ws = match ctx.resolve_target(state, target, TargetScope::SameProject) {
        Ok(target_ws) => target_ws,
        Err(_) => return Ok(RewriteScan::NoScan),
    };
    let is_ancestor = match ws.has_ancestor(&target_ws, meta) {
        Ok(is_ancestor) => is_ancestor,
        Err(err) => {
            out.warn(&err.to_string());
            return Ok(RewriteScan::NoScan);
        }
    };
    Ok(if is_ancestor {
        RewriteScan::Scan {
            start: ws.clone(),
            bound: Some(target_ws),
        }
    } else {
        RewriteScan::NoScan
    })
}

/// Warns when the base workspace of the range sits above `ws` across a rewritten edge. See
/// [`range_rewrite_scan`] for how each range flag maps to a scan. It reads the metadata only when a
/// range flag is set, so a range with no flag skips the read.
fn warn_if_range_parent_rewritten(
    ctx: &Context,
    state: &GlobalState,
    ws: &Workspace,
    range: &RangeArgs,
    out: &mut output::Stdio,
) -> Result<()> {
    if !range.is_set() {
        return Ok(());
    }
    let meta = ctx.scope.project().metadata().read()?;
    if let RewriteScan::Scan { start, bound } =
        range_rewrite_scan(ctx, state, ws, range, &meta, out)?
    {
        warn_if_parent_rewritten(&start, &meta, bound.as_ref(), out);
    }
    Ok(())
}

/// The client settings of a pass-through git run. In JSON mode the machine result is not a
/// terminal, so an automatic color choice means no color. An explicit `--color` still wins.
fn capture_settings<R: io::Write, D: io::Write, M: io::Write>(
    cli: &Cli,
    settings: ClientSettings,
    out: &output::Channel<R, D, M>,
) -> ClientSettings {
    if out.json() {
        cli.settings(false, false)
    } else {
        settings
    }
}

/// Writes the machine result of a pass-through git command. A failed git run gives one error
/// document and a nonzero process status.
fn report_passthrough<R: io::Write, D: io::Write, M: io::Write>(
    out: &mut output::Channel<R, D, M>,
    command: &str,
    rev: Option<&str>,
    args: &[String],
    run: &subspace::worktree::CapturedGit,
) -> Result<()> {
    if !run.status.success() {
        let code = run
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        out.failure("git", &format!("git {command} exited with status {code}"));
        return Ok(());
    }
    out.success(&machine::GitPassthroughJson {
        command,
        range: rev,
        args,
        stdout: &run.stdout,
    })
}

/// Formats a process exit status as `<code>` or `signal` when the process died from a signal.
fn exit_code(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

fn cmd_diff(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    settings: ClientSettings,
    range: &RangeArgs,
    args: &[String],
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let wt = ws.worktree()?;
    warn_if_range_parent_rewritten(&ctx, state, ws, range, out)?;
    let (rev, extra) = split_target(&ctx, state, range, args)?;
    let (settings, extra) =
        take_passthrough_color(capture_settings(cli, settings, out), out.json(), extra);
    let mut diff_args = vec![git_color_arg(settings)];
    if let Some(output) = range.output {
        diff_args.push(
            match output {
                OutputMode::Patch => "--patch",
                OutputMode::Stat => "--stat",
                OutputMode::NameOnly => "--name-only",
            }
            .to_string(),
        );
    }
    diff_args.extend(extra);
    if !range.paths.is_empty() {
        diff_args.push("--".to_string());
        diff_args.extend(range.paths.iter().cloned());
    }
    if out.json() {
        let run = wt.diff_captured(rev.as_deref(), &diff_args)?;
        return report_passthrough(out, "diff", rev.as_deref(), &diff_args, &run);
    }
    let st = wt.diff(rev.as_deref(), &diff_args)?;
    if !st.success() {
        process::exit(st.code().unwrap_or(1));
    }
    Ok(())
}

fn cmd_log(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    settings: ClientSettings,
    range: &RangeArgs,
    args: &[String],
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let wt = ws.worktree()?;
    warn_if_range_parent_rewritten(&ctx, state, ws, range, out)?;
    let (rev, extra) = split_target(&ctx, state, range, args)?;
    let (settings, extra) =
        take_passthrough_color(capture_settings(cli, settings, out), out.json(), extra);
    let mut log_args: Vec<String> = vec![git_color_arg(settings), "--reverse".to_string()];
    if let Some(output) = range.output {
        log_args.push(
            match output {
                OutputMode::Patch => "--patch",
                OutputMode::Stat => "--stat",
                OutputMode::NameOnly => "--name-only",
            }
            .to_string(),
        );
    }
    if rev.as_deref().is_some_and(|r| r.contains("...")) {
        log_args.push("--cherry-pick".to_string());
        log_args.push("--right-only".to_string());
    }
    log_args.extend(extra);
    if !range.paths.is_empty() {
        log_args.push("--".to_string());
        log_args.extend(range.paths.iter().cloned());
    }
    if out.json() {
        let run = wt.log_captured(rev.as_deref(), &log_args)?;
        return report_passthrough(out, "log", rev.as_deref(), &log_args, &run);
    }
    let st = wt.log(rev.as_deref(), &log_args)?;
    if !st.success() {
        process::exit(st.code().unwrap_or(1));
    }
    Ok(())
}

fn resolve_start(
    ctx: &Context,
    state: &GlobalState,
    project: &Project,
    git_ref: Option<&str>,
) -> Result<StartPoint> {
    Ok(match git_ref {
        None => {
            let origin = ctx
                .origin()?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no start ref given, and not inside a workspace\npass a ref to start from: a branch, a tag, or a SHA"
                    )
                })?;
            StartPoint::Commit(origin.worktree()?.resolve_ref("HEAD")?)
        }
        Some(r) if r.starts_with('@') => StartPoint::Commit(
            ctx.resolve_target(state, r, TargetScope::SameProject)?
                .worktree()?
                .resolve_ref("HEAD")?,
        ),
        Some(r) => project.anchor_worktree()?.resolve_start(r)?,
    })
}

/// The hook fields for `project`: its name, its path, and the env a hook gets (PROJECT_NAME,
/// PROJECT_PATH, and PROJECT_CONFIG).
fn project_fields(project: &Project) -> Fields {
    Fields::new(project.name(), project.path().to_string_lossy()).env(project.identity_env())
}

/// The hook fields for `workspace`: its project, its path, its name, and the env a hook gets
/// (PROJECT_NAME, PROJECT_PATH, PROJECT_CONFIG, and WORKSPACE_NAME).
fn workspace_fields(workspace: &Workspace) -> Fields {
    let mut env = workspace.project().identity_env();
    env.push(("WORKSPACE_NAME".to_string(), workspace.name().to_string()));
    Fields::new(
        workspace.project().name(),
        workspace.path().to_string_lossy(),
    )
    .workspace(workspace.name())
    .env(env)
}

fn cmd_add(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    args: &AddArgs,
    parent: Option<&str>,
    parent_of: Option<&str>,
) -> Result<()> {
    let AddArgs {
        name,
        git_ref,
        branch,
        detach,
        unstaged,
    } = args;
    let (branch, detach, unstaged) = (branch.as_deref(), *detach, *unstaged);

    let ctx = resolve_context(state, cli)?;
    let project = ctx.scope.project();

    validate_name(name)?;

    let new_ws = Workspace::new(project.clone(), name.to_string());
    // This check runs twice: once before the pre hooks, so they never fire for a name that is
    // already taken, and again under the lock, because these reads take no lock.
    if new_ws.path().exists() {
        bail!("workspace already exists: {}", name);
    }

    // For `--parent-of`, put the new workspace between the child and its former parent. The new
    // workspace takes that former parent, and the child anchors onto the new workspace later. See
    // `plan_insert_parent`, which runs once the new workspace exists.
    let (parent_ws, reparent_child, former_parent_edge) = match parent_of {
        Some(child) => {
            let child = ctx.resolve_target(state, child, TargetScope::SameProject)?;
            let meta = project.metadata().read()?;
            let former_parent_edge = child.parent_edge(&meta);
            let former_parent = child.parent_workspace(&meta);
            (former_parent, Some(child), former_parent_edge)
        }
        None => {
            let parent_ws = match parent {
                Some(p) => Some(ctx.resolve_target(state, p, TargetScope::SameProject)?),
                None => None,
            };
            (parent_ws, None, None)
        }
    };

    if unstaged && parent_ws.is_none() {
        bail!("--unstaged requires a parent workspace to copy working-tree state from");
    }

    let start = if let Some(former_parent) = parent_ws
        .as_ref()
        .filter(|_| reparent_child.is_some() && git_ref.is_none())
    {
        StartPoint::Commit(former_parent.worktree()?.resolve_ref("HEAD")?)
    } else {
        resolve_start(&ctx, state, project, git_ref.as_deref())?
    };

    // Both phases run the same fields, so they are built here, from the tree as it stands before
    // the change: the planned path of the new workspace, and the parent each edge moves from.
    let mut add_fields = workspace_fields(&new_ws);
    if let Some(parent_ws) = &parent_ws {
        add_fields = add_fields.parent(parent_ws.name());
    }
    let reparent_fields = reparent_child.as_ref().map(|child| {
        let mut fields = workspace_fields(child).parent(new_ws.name());
        if let Some(former_parent_edge) = &former_parent_edge {
            fields = fields.previous(former_parent_edge.name.clone());
        }
        fields
    });
    hooks.run(HookKind::WorkspaceAdd, Phase::Pre, &add_fields)?;
    if let Some(fields) = &reparent_fields {
        hooks.run(HookKind::WorkspaceReparent, Phase::Pre, fields)?;
    }

    let lock = project.lock(&lock_holder(&ctx.scope, &cli.command))?;

    // `former_parent_edge` and `parent_ws` were read before the lock, and `plan_insert_parent`
    // below feeds `former_parent_edge` into the new metadata. Re-read here and bail if the child's
    // edge moved, or its former parent is gone, since the plan captured above is now stale.
    if let Some(child) = &reparent_child {
        let meta = project.metadata().read()?;
        let parent_gone = parent_ws
            .as_ref()
            .is_some_and(|ws| Workspace::get(project, ws.name()).is_none());
        if child.parent_edge(&meta) != former_parent_edge || parent_gone {
            bail!("workspace changed since the add was planned\nrun it again");
        }
    }

    // Resolve the parent edge before anything lands on disk, so a failure here cannot leave half a
    // workspace behind. For `--parent-of`, `plan_insert_parent` plans the edge after creation
    // instead, because `start` is the tip of the former parent and cannot fail there.
    let parent_edge = if reparent_child.is_some() {
        None
    } else {
        match &parent_ws {
            Some(parent_ws) => match parent_ws.edge_for_child(&start) {
                Ok(edge) => Some(edge),
                Err(Error::NoCommonAncestor { .. }) => bail!(
                    "start point '{start}' shares no history with parent workspace '{}'",
                    parent_ws.name()
                ),
                Err(error) => return Err(error.into()),
            },
            None => None,
        }
    };

    if new_ws.path().exists() {
        bail!("workspace already exists: {}", name);
    }

    let anchor = project.anchor_worktree()?;
    if detach {
        new_ws.checkout_detached(&lock, &anchor, &start)?;
    } else {
        match &start {
            StartPoint::Branch(existing) if branch.is_none() => {
                new_ws.checkout_branch(&lock, &anchor, existing.name())?
            }
            StartPoint::RemoteBranch(remote) => {
                new_ws.create_tracking_branch(&lock, &anchor, branch, remote.name())?
            }
            _ => new_ws.create_branch(&lock, &anchor, branch, &start)?,
        }
    }

    let actual_branch = new_ws.worktree()?.get_branch();

    // The edge of the child onto the new workspace is a new branch base off the current tip of the
    // child, not the old base into its former parent. The history of the new workspace can diverge
    // from the history of that former parent before it reaches the first anchor of the child.
    let (parent_edge, reparent_child_edge) = match &reparent_child {
        Some(child) => {
            let (new_ws_edge, child_edge) =
                plan_insert_parent(child, former_parent_edge.as_ref(), &new_ws)?;
            (new_ws_edge, Some(child_edge))
        }
        None => (parent_edge, None),
    };

    if parent_edge.is_some() || actual_branch.is_some() || reparent_child.is_some() {
        project.metadata().update(&lock, |meta| {
            let ws_meta = meta
                .workspaces
                .entry(name.to_string())
                .or_insert_with(WorkspaceMeta::default);
            if let Some(branch) = &actual_branch {
                ws_meta.branch = Some(branch.clone());
            }
            if let Some(parent_edge) = &parent_edge {
                ws_meta.parent = Some(parent_edge.clone());
            }
            if let (Some(child), Some(edge)) = (&reparent_child, &reparent_child_edge) {
                meta.workspaces
                    .entry(child.name().to_string())
                    .or_default()
                    .parent = Some(edge.clone());
            }
            Ok(())
        })?;
    }

    if unstaged {
        let parent_ws = parent_ws
            .as_ref()
            .expect("checked --unstaged requires parent");
        parent_ws
            .worktree()?
            .merge_working_tree(&lock, &new_ws.worktree()?)?;
    }

    drop(lock);
    let mut pending = vec![(HookKind::WorkspaceAdd, add_fields)];
    if let Some(fields) = reparent_fields {
        pending.push((HookKind::WorkspaceReparent, fields));
    }
    run_post_hooks(hooks, &pending)?;

    if out.json() {
        let reparented = match (&reparent_child, &reparent_child_edge) {
            (Some(child), Some(edge)) => vec![machine::ReparentJson {
                workspace: child.name(),
                parent: Some(edge.name.as_str()),
            }],
            _ => Vec::new(),
        };
        out.success(&machine::WorkspaceCreatedJson {
            project: project.name(),
            workspace: new_ws.name(),
            path: new_ws.path().display().to_string(),
            branch: actual_branch.as_deref(),
            parent: parent_edge.as_ref().map(|edge| edge.name.as_str()),
            reparented,
        })?;
    }

    Ok(())
}

/// The hook fields a removal earns, as one `workspace_remove` field set per target and one
/// `workspace_reparent` field set per child the removal moves.
///
/// Both phases run the same fields, so each pass builds them from `meta` as it stands before the
/// change. A removed workspace carries the parent it had, and a moved child carries its new parent
/// and the parent it leaves.
fn removal_fields(
    project: &Project,
    targets: &[Workspace],
    meta: &ProjectMeta,
    plan: &ReparentPlan,
) -> (Vec<Fields>, Vec<Fields>) {
    let removed = targets
        .iter()
        .map(|ws| {
            let mut fields = workspace_fields(ws);
            if let Some(parent) = ws.parent_workspace(meta) {
                fields = fields.parent(parent.name());
            }
            fields
        })
        .collect();
    let reparented = plan
        .reparented()
        .map(|(child, parent)| {
            let child_ws = Workspace::new(project.clone(), child.to_string());
            let mut fields = workspace_fields(&child_ws);
            if let Some(parent) = parent {
                fields = fields.parent(parent);
            }
            if let Some(previous) = child_ws.parent_workspace(meta) {
                fields = fields.previous(previous.name());
            }
            fields
        })
        .collect();
    (removed, reparented)
}

/// The workspace removals that Git confirmed, with their metadata changes and pending post hooks.
struct ConfirmedRemovals {
    removed: Vec<machine::RemovedWorkspaceJson>,
    reparented: Vec<(String, Option<String>)>,
    pending: Vec<(HookKind, Fields)>,
    failures: Vec<String>,
}

/// Removes each target worktree and changes metadata only after Git confirms that removal.
fn remove_confirmed_workspaces(
    lock: &ProjectLock,
    project: &Project,
    targets: &[Workspace],
    delete_branch: bool,
) -> Result<ConfirmedRemovals> {
    let mut removed = Vec::new();
    let mut reparented = Vec::new();
    let mut pending = Vec::new();
    let mut failures = Vec::new();
    for ws in targets {
        let wt = match ws.worktree() {
            Ok(wt) => wt,
            Err(error) => {
                failures.push(format!("{}: {error:#}", ws.full_name()));
                continue;
            }
        };
        let branch = wt.get_branch();
        if let Err(error) = wt.remove_worktree(lock) {
            failures.push(format!("{}: {error:#}", ws.full_name()));
            continue;
        }

        let meta = project.metadata().read()?;
        let plan = plan_reparents(std::slice::from_ref(ws), &meta)?;
        let (removed_fields, reparented_fields) =
            removal_fields(project, std::slice::from_ref(ws), &meta, &plan);
        project.metadata().update(lock, |meta| {
            plan.apply(meta);
            meta.workspaces.remove(ws.name());
            Ok(())
        })?;

        let mut deleted_branch = None;
        if delete_branch && let Some(branch_name) = branch.clone() {
            match project.delete_branch(lock, &branch_name) {
                Ok(()) => deleted_branch = Some(branch_name),
                Err(e) => eprintln!("git branch delete failed: {}", e),
            }
        }
        let ws_state = ws.state_path();
        if ws_state.exists() {
            std::fs::remove_dir_all(&ws_state)?;
        }
        removed.push(machine::RemovedWorkspaceJson {
            workspace: ws.name().to_string(),
            path: ws.path().display().to_string(),
            branch,
            deleted_branch,
        });
        reparented.extend(
            plan.reparented()
                .map(|(child, parent)| (child.to_string(), parent.map(str::to_string))),
        );
        pending.extend(
            removed_fields
                .into_iter()
                .map(|fields| (HookKind::WorkspaceRemove, fields)),
        );
        pending.extend(
            reparented_fields
                .into_iter()
                .map(|fields| (HookKind::WorkspaceReparent, fields)),
        );
    }
    Ok(ConfirmedRemovals {
        removed,
        reparented,
        pending,
        failures,
    })
}

#[allow(clippy::too_many_arguments)]
fn cmd_remove<R: io::Write, D: io::Write, M: io::Write>(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    settings: ClientSettings,
    out: &mut output::Channel<R, D, M>,
    target: &str,
    keep_branch: bool,
    recursive: bool,
) -> Result<()> {
    let target_ws = resolve_workspace(state, cli, target)?;
    let project = target_ws.project();
    let scope = Scope::Workspace(target_ws.clone());
    let delete_branch = !keep_branch;

    // This runs twice: once for the prompt, and again under the lock. The prompt runs with no lock,
    // and the tree can change under it.
    let subtree = |ws: &Workspace| -> Result<Vec<Workspace>> {
        Ok(if recursive {
            WorkspaceTree::from_workspace(ws)?.post_order(ws)
        } else {
            vec![ws.clone()]
        })
    };
    let targets = subtree(&target_ws)?;

    if settings.interactive {
        let names: Vec<String> = targets.iter().map(|ws| ws.full_name()).collect();
        let prompt = if targets.len() > 1 {
            format!(
                "remove {} workspaces ({})? [y/N] ",
                targets.len(),
                names.join(", ")
            )
        } else {
            format!("remove {}? [y/N] ", names[0])
        };
        if report_declined(
            confirm(settings, &prompt)?,
            &format!("{} was not removed", names[0]),
            out,
        ) {
            return Ok(());
        }
    }

    // The pre hooks run before the lock, and every one of them runs before the first change. The
    // locked pass plans again, because these reads take no lock, and bails if the plan it finds
    // there differs from this one.
    let (pre_removed, pre_reparented) = {
        let meta = project.metadata().read()?;
        if let Some(default_name) = &meta.default
            && targets.iter().any(|ws| ws.name() == default_name)
        {
            return Err(Error::IsDefaultWorkspace.into());
        }
        let plan = plan_reparents(&targets, &meta)?;
        removal_fields(project, &targets, &meta, &plan)
    };
    for fields in &pre_removed {
        hooks.run(HookKind::WorkspaceRemove, Phase::Pre, fields)?;
    }
    for fields in &pre_reparented {
        hooks.run(HookKind::WorkspaceReparent, Phase::Pre, fields)?;
    }

    let lock = project.lock(&lock_holder(&scope, &cli.command))?;
    let now = subtree(&target_ws)?;
    if now
        .iter()
        .map(Workspace::name)
        .ne(targets.iter().map(Workspace::name))
    {
        bail!("workspaces changed since the removal was planned\nrun it again");
    }
    let meta = project.metadata().read()?;
    if let Some(default_name) = &meta.default
        && targets.iter().any(|ws| ws.name() == default_name)
    {
        return Err(Error::IsDefaultWorkspace.into());
    }
    let plan = plan_reparents(&targets, &meta)?;
    let (removed_fields, reparented_fields) = removal_fields(project, &targets, &meta, &plan);
    if (removed_fields.as_slice(), reparented_fields.as_slice())
        != (pre_removed.as_slice(), pre_reparented.as_slice())
    {
        bail!("workspaces changed since the removal was planned\nrun it again");
    }

    let ConfirmedRemovals {
        removed,
        reparented,
        pending,
        mut failures,
    } = remove_confirmed_workspaces(&lock, project, &targets, delete_branch)?;

    drop(lock);

    let hooks_result = run_post_hooks(hooks, &pending);
    if let Err(error) = hooks_result {
        failures.push(error.to_string());
    }
    if !failures.is_empty() {
        bail!("failed to remove workspaces:\n{}", failures.join("\n"));
    }

    if out.json() {
        let reparented = reparented
            .iter()
            .map(|(workspace, parent)| machine::ReparentJson {
                workspace,
                parent: parent.as_deref(),
            })
            .collect();
        out.success(&machine::WorkspaceRemovedJson {
            project: project.name(),
            removed,
            reparented,
        })?;
    }

    Ok(())
}

fn cmd_rename(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    target: &str,
    new_name: &str,
) -> Result<()> {
    let ws = resolve_workspace(state, cli, target)?;
    let project = ws.project().clone();
    let scope = Scope::Workspace(ws.clone());

    validate_name(new_name)?;

    let new_ws = Workspace::new(project.clone(), new_name.to_string());
    // This check runs twice: once for a fast failure before the prompt, and again under the lock,
    // because the prompt runs with no lock.
    let ensure_target_free = || -> Result<()> {
        if new_ws.path().exists() {
            bail!("workspace already exists: {}", new_name);
        }
        if new_ws.state_path().exists() {
            bail!(
                "workspace state directory already exists: {}",
                new_ws.state_path().display()
            );
        }
        Ok(())
    };
    ensure_target_free()?;

    // Both phases run the same fields, built from the tree as it stands before the change.
    let fields = workspace_fields(&new_ws)
        .previous(ws.name())
        .previous_path(ws.path().to_string_lossy());
    hooks.run(HookKind::WorkspaceRename, Phase::Pre, &fields)?;

    let lock = project.lock(&lock_holder(&scope, &cli.command))?;
    ensure_target_free()?;
    let previous_path = ws.path().display().to_string();
    std::fs::rename(ws.path(), new_ws.path())?;

    let new_wt = new_ws.worktree()?;
    new_wt.repair(&lock)?;

    project.metadata().update(&lock, |meta| {
        meta.rename_workspace(ws.name(), new_name);
        Ok(())
    })?;

    let old_state = ws.state_path();
    if old_state.exists() {
        std::fs::rename(&old_state, new_ws.state_path())?;
    }

    out.note(&format!("renamed to {}/{}", project.name(), new_name));

    drop(lock);

    hooks.run(HookKind::WorkspaceRename, Phase::Post, &fields)?;

    if out.json() {
        out.success(&machine::WorkspaceRenameJson {
            project: project.name(),
            workspace: new_name,
            previous: ws.name(),
            path: new_ws.path().display().to_string(),
            previous_path,
        })?;
    }

    Ok(())
}

/// The hook fields a repair pass earns, as one `workspace_remove` field set per pruned entry and
/// one `workspace_reparent` field set per child the plan moves.
///
/// Both phases run the same fields, so each pass builds them from `meta` as it stands before the
/// change. A pruned entry has no worktree left to name a parent from, so it carries none.
fn repair_fields(
    project: &Project,
    plan: &RepairPlan,
    meta: &ProjectMeta,
) -> (Vec<Fields>, Vec<Fields>) {
    let removed = plan
        .pruned()
        .iter()
        .map(|name| workspace_fields(&Workspace::new(project.clone(), name.clone())))
        .collect();
    let reparented = plan
        .reparented()
        .map(|(child, parent)| {
            let child_ws = Workspace::new(project.clone(), child.to_string());
            let mut fields = workspace_fields(&child_ws);
            if let Some(parent) = parent {
                fields = fields.parent(parent);
            }
            if let Some(previous) = child_ws.parent_workspace(meta) {
                fields = fields.previous(previous.name());
            }
            fields
        })
        .collect();
    (removed, reparented)
}

fn repair_relink_fields(project: &Project, relinks: &[subspace::workspace::Relink]) -> Vec<Fields> {
    relinks
        .iter()
        .map(|relink| {
            let workspace = Workspace::new(project.clone(), relink.workspace.clone());
            workspace_fields(&workspace)
        })
        .collect()
}

fn cmd_repair(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    name: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let scope = match name {
        Some(name) => Scope::Project(registered_project(state, name)?),
        None => resolve_context(state, cli)?.scope,
    };
    let project = scope.project().clone();

    if dry_run {
        let meta = project.metadata().read()?;
        let (anchor, relinks, plan) = plan_repair_pass(&project, &meta, out)?;
        report_repair(&plan, &relinks, out, true, anchor.is_some());
        machine::report_repair_json(&project, &plan, &relinks, out, true, anchor.is_some())?;
        return Ok(());
    }

    // The pre hooks run before the lock, and every one of them runs before the first change. This
    // pass reports nothing, because the locked pass reports the plan. The locked pass plans again,
    // because this read takes no lock, and bails if the plan it finds there differs from this one.
    let (pre_removed, pre_reparented, pre_relinks) = {
        let meta = project.metadata().read()?;
        let (_, relinks, plan) = plan_repair_pass(&project, &meta, out)?;
        let (removed, reparented) = repair_fields(&project, &plan, &meta);
        for fields in &removed {
            hooks.run(HookKind::WorkspaceRemove, Phase::Pre, fields)?;
        }
        for fields in &reparented {
            hooks.run(HookKind::WorkspaceReparent, Phase::Pre, fields)?;
        }
        for fields in repair_relink_fields(&project, relinks.relinks()) {
            hooks.run(HookKind::WorkspaceRelink, Phase::Pre, &fields)?;
        }
        (removed, reparented, relinks)
    };

    let lock = project.lock(&lock_holder(&scope, &cli.command))?;
    let meta = project.metadata().read()?;
    let (anchor, relinks, plan) = plan_repair_pass(&project, &meta, out)?;
    let (removed, reparented) = repair_fields(&project, &plan, &meta);
    if (removed.as_slice(), reparented.as_slice(), &relinks)
        != (
            pre_removed.as_slice(),
            pre_reparented.as_slice(),
            &pre_relinks,
        )
    {
        bail!("repair plan changed since the pre-hooks ran, run it again");
    }

    let relinks = match &anchor {
        Some(anchor) => apply_relinks(&project, relinks, anchor, &lock)?,
        None => relinks,
    };

    if !plan.is_empty() {
        project.metadata().update(&lock, |meta| {
            plan.apply(meta);
            Ok(())
        })?;
    }

    report_repair(&plan, &relinks, out, false, anchor.is_some());

    drop(lock);

    let pending: Vec<(HookKind, Fields)> = removed
        .into_iter()
        .map(|fields| (HookKind::WorkspaceRemove, fields))
        .chain(
            reparented
                .into_iter()
                .map(|fields| (HookKind::WorkspaceReparent, fields)),
        )
        .chain(
            repair_relink_fields(&project, relinks.relinks())
                .into_iter()
                .map(|fields| (HookKind::WorkspaceRelink, fields)),
        )
        .collect();
    run_post_hooks(hooks, &pending)?;

    machine::report_repair_json(&project, &plan, &relinks, out, false, anchor.is_some())?;
    Ok(())
}

/// Plans both halves of a repair pass, the worktree-link rebuilds and the metadata corrections,
/// against one metadata snapshot.
///
/// A project with no worktree that opens gets a notice on the terminal and an empty link plan,
/// because the link half needs a repository to read. The metadata half runs either way.
fn plan_repair_pass(
    project: &Project,
    meta: &ProjectMeta,
    out: &mut output::Stdio,
) -> Result<(Option<Worktree>, RelinkPlan, RepairPlan)> {
    let anchor = match project.anchor_worktree() {
        Ok(anchor) => Some(anchor),
        Err(e) => {
            out.note(&format!("cannot repair worktree links: {e}"));
            None
        }
    };
    let relinks = match &anchor {
        Some(anchor) => plan_relinks(project, meta, anchor)?,
        None => RelinkPlan::default(),
    };
    let plan = plan_repair(project, meta)?;
    Ok((anchor, relinks, plan))
}

/// Prints what a repair pass found and did. `links_checked` says whether the link half ran. A pass
/// that skipped it must not report that the links are good.
fn report_repair(
    plan: &RepairPlan,
    relinks: &RelinkPlan,
    out: &mut output::Stdio,
    dry_run: bool,
    links_checked: bool,
) {
    if plan.is_empty() && relinks.is_empty() {
        let line = if !links_checked {
            "nothing to repair in the metadata"
        } else if dry_run {
            "nothing to repair"
        } else {
            "nothing to repair (worktree links checked)"
        };
        out.note(line);
        return;
    }
    let verb = if dry_run { "will rebuild" } else { "rebuilt" };
    for relink in relinks.relinks() {
        out.note(&format!(
            "{verb} worktree link: {} (branch {})",
            relink.workspace, relink.branch
        ));
    }
    for (name, why) in relinks.unrepairable() {
        let reason = machine::unrepairable_json(name, why).message;
        out.note(&format!("cannot rebuild worktree link: {name} ({reason})"));
    }
    let verb = if dry_run { "will prune" } else { "pruned" };
    for name in plan.pruned() {
        out.note(&format!("{verb} dead workspace metadata: {name}"));
    }
    let verb = if dry_run {
        "will reparent"
    } else {
        "reparented"
    };
    for (child, parent) in plan.reparented() {
        out.note(&format!(
            "{verb} {child} onto {}",
            parent.unwrap_or("<root>")
        ));
    }
    if let Some(name) = plan.stale_default() {
        let verb = if dry_run { "will clear" } else { "cleared" };
        out.note(&format!("{verb} stale default: {name}"));
    }
}

/// Refuses to replace recovery state from an unfinished integration.
fn require_no_integration(meta: &ProjectMeta, workspace: &Workspace) -> Result<()> {
    if meta
        .workspaces
        .get(workspace.name())
        .is_some_and(|entry| entry.integration.is_some())
    {
        bail!(
            "unfinished integration for workspace {}; use integration continue or abort",
            workspace.name()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_merge(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    settings: ClientSettings,
    out: &mut output::Stdio,
    source: &str,
    squash: Option<&str>,
    all: bool,
    unstaged: bool,
    remove: bool,
    delete_branch: bool,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let dest_ws = ctx.scope.require_workspace()?;
    let source_ws = ctx.resolve_target(state, source, TargetScope::SameProject)?;
    let project = ctx.scope.project();
    let lock = project.lock(&lock_holder(&ctx.scope, &cli.command))?;

    let dest_wt = dest_ws.worktree()?;

    if source_ws.path() == dest_ws.path() {
        bail!("source and base resolve to the same workspace");
    }

    let meta = project.metadata().read()?;
    require_no_integration(&meta, dest_ws)?;
    let original_base = dest_ws.parent_edge(&meta);
    require_merge_chains_current(dest_ws, &source_ws, &meta)?;

    let base_head = dest_wt.head()?;
    let source_wt = source_ws.worktree()?;
    let source_head = source_wt.head()?;

    let (commits, _, _) = merge_commits(
        &dest_wt,
        &base_head,
        &source_head,
        source_ws.parent_edge(&meta).as_ref(),
        all,
    )?;

    if settings.interactive {
        let no_changes = if unstaged {
            let status = source_wt.status()?;
            status.index.is_empty() && status.worktree.is_empty()
        } else {
            true
        };
        if commits.is_empty() && no_changes {
            out.diagnostic("nothing to merge");
            out.success(&machine::MergeJson {
                project: project.name(),
                workspace: dest_ws.name(),
                source: source_ws.name(),
                squash: squash.is_some(),
                unstaged,
                commits: Vec::new(),
                removed: None,
            })?;
            return Ok(());
        }
        let title = format!(
            "Merge {} into {}",
            source_ws.full_name(),
            dest_ws.full_name()
        );
        let title = if settings.color {
            format!("\x1b[1m{title}\x1b[0m")
        } else {
            title
        };
        if out.json() {
            out.report(&title);
            out.report("");
        } else {
            println!("{title}\n");
        }
        let log_args = ["-1".to_string(), git_color_arg(settings)];
        for commit in &commits {
            if out.json() {
                let run = dest_wt.log_captured(Some(&commit.sha()), &log_args)?;
                for line in run.stdout.lines() {
                    out.diagnostic(line);
                }
            } else {
                dest_wt.log(Some(&commit.sha()), &log_args)?;
            }
        }
        if report_declined(
            confirm(
                settings,
                &format!("\nmerge into {}? [y/N] ", dest_ws.full_name()),
            )?,
            "the merge was declined",
            out,
        ) {
            return Ok(());
        }
    }

    if squash.is_some() && commits.is_empty() {
        bail!("nothing to merge");
    }

    project.metadata().update(&lock, |metadata| {
        if let Some(entry) = metadata.workspaces.get_mut(dest_ws.name()) {
            entry.integration = Some(IntegrationState {
                original_base,
                target: squash.map(|_| subspace::metadata::ParentEdge {
                    name: dest_ws.name().to_string(),
                    base: base_head.commit(),
                }),
                mode: "merge".to_string(),
                squash_message: squash.map(str::to_string),
            });
        }
        Ok(())
    })?;

    match squash {
        None => {
            if !commits.is_empty() {
                if out.json() {
                    let run = dest_wt.cherry_pick_commits_captured(&lock, &commits)?;
                    for line in run.stdout.lines() {
                        out.note(line);
                    }
                    if !run.status.success() {
                        bail!("git exited with status {}", exit_code(&run.status));
                    }
                } else {
                    dest_wt.cherry_pick_commits(&lock, &commits, settings)?;
                }
            }
            if unstaged {
                source_wt.merge_working_tree(&lock, &dest_wt)?;
            }
        }
        Some(message) => {
            let run = dest_wt.stage_commits_captured(&lock, &commits)?;
            for line in run.stdout.lines() {
                out.note(line);
            }
            if !run.status.success() {
                bail!("git exited with status {}", exit_code(&run.status));
            }
            if dest_wt.status()?.index.is_empty() {
                project.metadata().update(&lock, |metadata| {
                    if let Some(entry) = metadata.workspaces.get_mut(dest_ws.name()) {
                        entry.integration = None;
                    }
                    Ok(())
                })?;
                bail!("nothing to merge");
            }
            let run = dest_wt.create_commit_captured(&lock, message)?;
            for line in run.stdout.lines() {
                out.note(line);
            }
            if !run.status.success() {
                bail!("git exited with status {}", exit_code(&run.status));
            }
            if unstaged {
                source_wt.merge_working_tree(&lock, &dest_wt)?;
            }
        }
    }

    let should_remove = if remove && settings.interactive {
        confirm(
            settings,
            &format!("remove {}? [y/N] ", source_ws.full_name()),
        )?
    } else {
        remove
    };
    let mut removed = None;
    let pending_hooks = if should_remove {
        // The removal is decided after the merge, so the pre hooks run here, before the removal
        // and after the merge, rather than before the lock.
        let meta = project.metadata().read()?;
        let plan = plan_reparents(std::slice::from_ref(&source_ws), &meta)?;
        let (removed_fields, reparented_fields) =
            removal_fields(project, std::slice::from_ref(&source_ws), &meta, &plan);
        for fields in &removed_fields {
            hooks.run(HookKind::WorkspaceRemove, Phase::Pre, fields)?;
        }
        for fields in &reparented_fields {
            hooks.run(HookKind::WorkspaceReparent, Phase::Pre, fields)?;
        }
        let (record, pending) = remove_merged_source(
            &lock,
            project,
            &source_ws,
            delete_branch,
            plan,
            removed_fields,
            reparented_fields,
            out,
        )?;
        removed = Some(record);
        pending
    } else {
        Vec::new()
    };

    project.metadata().update(&lock, |metadata| {
        if let Some(entry) = metadata.workspaces.get_mut(dest_ws.name()) {
            entry.integration = None;
        }
        Ok(())
    })?;
    drop(lock);
    run_post_hooks(hooks, &pending_hooks)?;

    out.success(&machine::MergeJson {
        project: project.name(),
        workspace: dest_ws.name(),
        source: source_ws.name(),
        squash: squash.is_some(),
        unstaged,
        commits,
        removed,
    })?;

    Ok(())
}

/// Removes the merged source workspace, and returns its machine record with the hooks that the
/// removal leaves pending.
#[allow(clippy::too_many_arguments)]
fn remove_merged_source<R: io::Write, D: io::Write, M: io::Write>(
    lock: &ProjectLock,
    project: &Project,
    source_ws: &Workspace,
    delete_branch: bool,
    plan: ReparentPlan,
    removed_fields: Vec<Fields>,
    reparented_fields: Vec<Fields>,
    out: &mut output::Channel<R, D, M>,
) -> Result<(machine::MergeRemovedJson, Vec<(HookKind, Fields)>)> {
    let reparented: Vec<machine::OwnedReparentJson> = plan
        .reparented()
        .map(|(child, parent)| machine::OwnedReparentJson {
            workspace: child.to_string(),
            parent: parent.map(str::to_string),
        })
        .collect();
    if project.metadata().read()?.default.as_deref() == Some(source_ws.name()) {
        return Err(Error::IsDefaultWorkspace.into());
    }
    let wt = source_ws.worktree()?;
    let branch = wt.get_branch();
    let path = source_ws.path().display().to_string();
    wt.remove_worktree(lock)?;
    project.metadata().update(lock, |meta| {
        if meta.default.as_deref() == Some(source_ws.name()) {
            return Err(Error::IsDefaultWorkspace);
        }
        plan.apply(meta);
        meta.workspaces.remove(source_ws.name());
        Ok(())
    })?;
    let source_state = source_ws.state_path();
    if source_state.exists() {
        std::fs::remove_dir_all(&source_state)?;
    }
    let mut deleted_branch = None;
    if delete_branch && let Some(branch_name) = branch.clone() {
        match project.delete_branch(lock, &branch_name) {
            Ok(()) => deleted_branch = Some(branch_name),
            Err(e) => out.diagnostic(&format!("git branch delete failed: {}", e)),
        }
    }
    let record = machine::MergeRemovedJson {
        workspace: source_ws.name().to_string(),
        path,
        branch,
        deleted_branch,
        reparented,
    };
    let pending = removed_fields
        .into_iter()
        .map(|fields| (HookKind::WorkspaceRemove, fields))
        .chain(
            reparented_fields
                .into_iter()
                .map(|fields| (HookKind::WorkspaceReparent, fields)),
        )
        .collect();
    Ok((record, pending))
}

/// Prompts on stderr and reads a yes or no answer from stdin. Returns `false` when the run is not
/// interactive.
fn confirm(settings: ClientSettings, prompt: &str) -> Result<bool> {
    if !settings.interactive {
        return Ok(false);
    }
    eprint!("{prompt}");
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

fn complete_rebase_recovery(
    worktree: &Worktree,
    lock: &ProjectLock,
    continue_rebase: bool,
) -> Result<&'static str> {
    let args = if continue_rebase {
        ["--continue"]
    } else {
        ["--abort"]
    };
    let result = worktree.rebase_captured(lock, &args)?;
    if !result.status.success() {
        bail!(
            "git rebase failed with status {}",
            exit_code(&result.status)
        );
    }
    if worktree.operation_in_progress().is_some() {
        bail!("git rebase did not complete");
    }
    Ok(if continue_rebase {
        "reanchored"
    } else {
        "aborted"
    })
}

fn complete_merge_recovery(
    worktree: &Worktree,
    lock: &ProjectLock,
    squash_message: Option<&str>,
    starting_head: Option<&Sha>,
    continue_merge: bool,
) -> Result<&'static str> {
    let in_progress = worktree.operation_in_progress().is_some();
    let result = match (squash_message, continue_merge, in_progress) {
        (Some(_), true, true) | (Some(_), false, true) | (None, _, _) => worktree
            .cherry_pick_captured(
                lock,
                &[if continue_merge {
                    "--continue"
                } else {
                    "--abort"
                }],
            )?,
        (Some(_), true, false) => worktree.soft_reset_captured(
            lock,
            starting_head.ok_or_else(|| anyhow::anyhow!("squash merge has no starting commit"))?,
        )?,
        (Some(_), false, false) => worktree.reset_merge_captured(lock)?,
    };
    if !result.status.success() {
        bail!("git merge failed with status {}", exit_code(&result.status));
    }
    if worktree.operation_in_progress().is_some() {
        bail!("git merge did not complete");
    }
    if let (Some(message), true) = (squash_message, continue_merge) {
        let starting_head =
            starting_head.ok_or_else(|| anyhow::anyhow!("squash merge has no starting commit"))?;
        if in_progress {
            let run = worktree.soft_reset_captured(lock, starting_head)?;
            if !run.status.success() {
                bail!("git reset failed with status {}", exit_code(&run.status));
            }
        }
        let run = worktree.create_commit_captured(lock, message)?;
        if !run.status.success() {
            bail!("git commit failed with status {}", exit_code(&run.status));
        }
    }
    Ok(if continue_merge {
        "continued"
    } else {
        "aborted"
    })
}

#[derive(Debug)]
struct IntegrationRecovery {
    operation: &'static str,
    outcome: &'static str,
}

fn finish_integration_recovery(
    project: &Project,
    workspace: &Workspace,
    worktree: &Worktree,
    lock: &ProjectLock,
    integration: &IntegrationState,
    continue_integration: bool,
) -> Result<IntegrationRecovery> {
    let (operation, outcome, parent) = if integration.mode == "merge" {
        (
            "merge",
            complete_merge_recovery(
                worktree,
                lock,
                integration.squash_message.as_deref(),
                integration.target.as_ref().map(|edge| &edge.base),
                continue_integration,
            )?,
            None,
        )
    } else {
        let (_interactive, _all) = integration
            .mode
            .strip_prefix("rebase:interactive=")
            .and_then(|mode| mode.split_once(",all="))
            .map(|(interactive, all)| (interactive == "true", all == "true"))
            .ok_or_else(|| anyhow::anyhow!("invalid stored rebase mode"))?;
        let target = integration
            .target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("stored rebase has no target"))?;
        (
            "rebase",
            complete_rebase_recovery(worktree, lock, continue_integration)?,
            if continue_integration {
                Some(Some(target.clone()))
            } else {
                Some(integration.original_base.clone())
            },
        )
    };
    project.metadata().update(lock, |metadata| {
        if let Some(entry) = metadata.workspaces.get_mut(workspace.name()) {
            if let Some(parent) = parent {
                entry.parent = parent;
            }
            entry.integration = None;
        }
        Ok(())
    })?;
    Ok(IntegrationRecovery { operation, outcome })
}

fn cmd_integration_recovery(
    state: &GlobalState,
    cli: &Cli,
    out: &mut output::Stdio,
    continue_integration: bool,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let project = ctx.scope.project();
    let lock = project.lock(&lock_holder(&ctx.scope, &cli.command))?;
    let wt = ws.worktree()?;
    let meta = project.metadata().read()?;
    let integration = meta
        .workspaces
        .get(ws.name())
        .and_then(|entry| entry.integration.clone())
        .ok_or_else(|| anyhow::anyhow!("no stopped integration for workspace {}", ws.name()))?;
    let recovery =
        finish_integration_recovery(project, ws, &wt, &lock, &integration, continue_integration)?;
    drop(lock);
    let head = ws.worktree()?.head()?.sha();
    out.success(&machine::IntegrationJson {
        project: project.name(),
        workspace: ws.name(),
        operation: recovery.operation,
        outcome: recovery.outcome,
        head,
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_rebase(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    settings: ClientSettings,
    out: &mut output::Stdio,
    base: Option<&str>,
    interactive: bool,
    all: bool,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let ws = ctx.scope.require_workspace()?;
    let project = ctx.scope.project();
    let lock = project.lock(&lock_holder(&ctx.scope, &cli.command))?;

    let wt = ws.worktree()?;

    let meta = project.metadata().read()?;
    require_no_integration(&meta, ws)?;
    let former_parent = ws.parent_workspace(&meta);
    let onto_ws = match base {
        Some(target) => ctx.resolve_target(state, target, TargetScope::SameProject)?,
        None => match &former_parent {
            Some(pw) => pw.clone(),
            None => bail!("no base specified and workspace has no parent"),
        },
    };
    let former_edge = ws.parent_edge(&meta);

    let onto_head = onto_ws.worktree()?.head()?;
    let head = wt.head()?;
    let mode = RebaseMode { interactive, all };
    let plan = ws.plan_rebase(&wt, &meta, &onto_ws, &onto_head, &head, mode)?;
    let new_edge = ParentEdge {
        name: onto_ws.name().to_string(),
        base: onto_head.commit(),
    };
    // The replay needs the base tip, which needs the lock, so the pre hook runs here rather than
    // before the lock. It runs only when the edge would move, and the post hook runs only when it
    // did.
    let moves_edge = former_edge.as_ref() != Some(&new_edge);
    let mut fields = workspace_fields(ws).parent(onto_ws.name());
    if let Some(former_parent) = &former_parent {
        fields = fields.previous(former_parent.name().to_string());
    }
    if moves_edge {
        hooks.run(HookKind::WorkspaceReparent, Phase::Pre, &fields)?;
    }
    project.metadata().update(&lock, |metadata| {
        if let Some(entry) = metadata.workspaces.get_mut(ws.name()) {
            entry.integration = Some(IntegrationState {
                original_base: former_edge.clone(),
                target: Some(new_edge.clone()),
                mode: format!("rebase:interactive={interactive},all={all}"),
                squash_message: None,
            });
        }
        Ok(())
    })?;

    let result = if out.json() && !interactive {
        let mut sink = |line: &str| out.diagnostic(line);
        ws.rebase_onto(
            &lock,
            &onto_ws,
            &onto_head,
            &plan,
            RebaseIo {
                interactive,
                quiet: settings.quiet(),
                on_output: Some(&mut sink),
            },
        )
    } else {
        ws.rebase_onto(
            &lock,
            &onto_ws,
            &onto_head,
            &plan,
            RebaseIo {
                interactive,
                quiet: settings.quiet(),
                on_output: None,
            },
        )
    };
    let mid_rebase = matches!(&result, Ok(RebaseOutcome::StoppedMidRebase))
        || matches!(
            wt.operation_in_progress(),
            Some(InProgressOperation::Rebase { .. })
        );
    if mid_rebase {
        let suggestion = match base {
            Some(target) if all => format!("subspace rebase {target} --all"),
            Some(target) => format!("subspace rebase {target}"),
            None => "subspace rebase".to_string(),
        };
        out.diagnostic(&format!(
            "after resolving with `git rebase --continue`, re-run `{suggestion}` to re-anchor"
        ));
    }
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if !matches!(
                wt.operation_in_progress(),
                Some(InProgressOperation::Rebase { .. })
            ) {
                project.metadata().update(&lock, |metadata| {
                    if let Some(entry) = metadata.workspaces.get_mut(ws.name()) {
                        entry.integration = None;
                    }
                    Ok(())
                })?;
            }
            return Err(error.into());
        }
    };
    if outcome == RebaseOutcome::Reanchored {
        project.metadata().update(&lock, |metadata| {
            if let Some(entry) = metadata.workspaces.get_mut(ws.name()) {
                entry.integration = None;
            }
            Ok(())
        })?;
    }
    drop(lock);
    if outcome == RebaseOutcome::Reanchored && moves_edge {
        hooks.run(HookKind::WorkspaceReparent, Phase::Post, &fields)?;
    }

    out.success(&machine::RebaseJson {
        project: project.name(),
        workspace: ws.name(),
        base: onto_ws.name(),
        base_head: onto_head.sha(),
        previous_head: head.sha(),
        all,
        outcome: match outcome {
            RebaseOutcome::Reanchored => "reanchored",
            RebaseOutcome::StoppedMidRebase => "stopped_mid_rebase",
        },
    })?;
    Ok(())
}

fn cmd_snapshot(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    settings: ClientSettings,
    out: &mut output::Stdio,
    action: Option<&SnapshotAction>,
) -> Result<()> {
    match action {
        None => cmd_snapshot_create(state, cli, settings, out, None),
        Some(SnapshotAction::Create { git_ref }) => {
            cmd_snapshot_create(state, cli, settings, out, git_ref.as_deref())
        }
        Some(SnapshotAction::List) => cmd_snapshot_list(state, cli, out),
        Some(SnapshotAction::Restore { file, name }) => {
            cmd_snapshot_restore(state, cli, hooks, out, file, name.as_deref())
        }
    }
}

fn cmd_snapshot_create(
    state: &GlobalState,
    cli: &Cli,
    settings: ClientSettings,
    out: &mut output::Stdio,
    git_ref: Option<&str>,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let workspace = ctx.scope.require_workspace()?;
    let worktree = workspace.worktree()?;
    // The scoped workspace resolves the ref, so `-w` and a relative ref agree on the commit that
    // the archive holds. A sigil names the HEAD of another workspace.
    let sha = match git_ref {
        None => worktree.resolve_ref("HEAD")?,
        Some(r) if r.starts_with('@') => ctx
            .resolve_target(state, r, TargetScope::SameProject)?
            .worktree()?
            .resolve_ref("HEAD")?,
        Some(r) => worktree.resolve_ref(r)?,
    };

    if !settings.quiet() {
        let status = worktree.status()?;
        let tracked_changes = !status.index.is_empty()
            || status
                .worktree
                .iter()
                .any(|(_, s)| *s != GitFileStatus::Untracked);
        if tracked_changes {
            out.warn("the snapshot holds the commit, so uncommitted changes stay behind");
        }
        if worktree.exports_ignore(&sha)? {
            out.warn("export-ignore is set, so the archive leaves files out");
        }
    }

    let snapshot = snapshot::create(workspace, &sha)?;
    if out.json() {
        out.success(&machine::SnapshotJson {
            workspace: snapshot.workspace(),
            sha: snapshot.sha(),
            path: snapshot.path(),
            bytes: snapshot.size()?,
        })?;
    } else {
        println!("{}", snapshot.path().display());
    }
    Ok(())
}

fn cmd_snapshot_list(state: &GlobalState, cli: &Cli, out: &mut output::Stdio) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let snapshots = snapshot::list(ctx.scope.project())?;

    if out.json() {
        let infos = snapshots
            .iter()
            .map(|snapshot| {
                Ok(machine::SnapshotJson {
                    workspace: snapshot.workspace(),
                    sha: snapshot.sha(),
                    path: snapshot.path(),
                    bytes: snapshot.size()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        out.success(&infos)?;
    } else {
        for snapshot in &snapshots {
            let name = snapshot
                .path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            println!("{name}  {}", snapshot.size()?);
        }
    }
    Ok(())
}

fn cmd_snapshot_restore(
    state: &GlobalState,
    cli: &Cli,
    hooks: &HookRunner,
    out: &mut output::Stdio,
    file: &str,
    name: Option<&str>,
) -> Result<()> {
    let ctx = resolve_context(state, cli)?;
    let project = ctx.scope.project();
    let snapshot = snapshot::find(project, file)?;
    let commit = snapshot.commit(project)?;

    let args = AddArgs {
        name: name.unwrap_or(snapshot.workspace()).to_string(),
        git_ref: Some(commit.sha()),
        branch: None,
        detach: false,
        unstaged: false,
    };
    cmd_add(state, cli, hooks, out, &args, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;
    use subspace::workspace::Unrepairable;

    #[test]
    fn integration_machine_gate_covers_every_integration_command() {
        for argv in [
            vec!["subspace", "merge", "work"],
            vec!["subspace", "merge", "work", "-m", "msg"],
            vec!["subspace", "merge", "work", "--remove", "--delete-branch"],
            vec!["subspace", "rebase"],
            vec!["subspace", "rebase", "root", "--all"],
            vec!["subspace", "snapshot"],
            vec!["subspace", "snapshot", "create"],
            vec!["subspace", "snapshot", "create", "HEAD~1"],
            vec!["subspace", "snapshot", "list"],
            vec!["subspace", "snapshot", "restore", "work.tar"],
        ] {
            assert!(
                json_supported(&Cli::parse_from(&argv).command),
                "{argv:?} has no machine result"
            );
        }
    }

    #[test]
    /// An interactive rebase holds the terminal, so JSON mode refuses it.
    fn integration_machine_gate_rejects_a_terminal_command() {
        for argv in [
            vec!["subspace", "rebase", "-i"],
            vec!["subspace", "rebase", "root", "-i"],
        ] {
            let cli = Cli::parse_from(&argv);
            assert!(
                !json_supported(&cli.command),
                "{argv:?} must not have a machine result"
            );
            let (code, message) = json_rejection(&cli.command);
            assert_eq!(code, "unsupported");
            assert!(message.contains("holds the terminal"), "{message}");
        }
    }

    #[test]
    /// A rejected command writes one error document and exits with a nonzero status.
    fn integration_machine_a_terminal_command_gives_its_error_document() {
        let cli = Cli::parse_from(["subspace", "rebase", "-i"]);
        let (code, message) = json_rejection(&cli.command);
        let mut buffer = Vec::new();
        {
            let mut channel = output::Channel::new(
                Vec::new(),
                Vec::new(),
                &mut buffer,
                true,
                subspace::client::Verbosity::Normal,
            );
            channel.failure(code, message);
            assert_eq!(channel.finish(), 1);
        }
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(buffer).unwrap().trim()).unwrap();
        assert_eq!(value["error"]["code"], "unsupported");
    }

    #[test]
    /// A declined merge prompt writes a `declined` failure document and exits with status 1.
    fn integration_machine_merge_reports_a_declined_prompt() {
        let mut buffer = Vec::new();
        {
            let mut channel = output::Channel::new(
                Vec::new(),
                Vec::new(),
                &mut buffer,
                true,
                subspace::client::Verbosity::Normal,
            );
            let stopped = report_declined(false, "the merge was declined", &mut channel);
            assert!(stopped);
            assert_eq!(channel.finish(), 1);
        }
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(buffer).unwrap().trim()).unwrap();
        assert_eq!(value["error"]["code"], "declined");
        assert_eq!(value["error"]["message"], "the merge was declined");
    }

    #[test]
    fn integration_machine_merge_names_its_commits_and_keeps_removal_null() {
        let value = to_value(&machine::MergeJson {
            project: "tide",
            workspace: "root",
            source: "work",
            squash: false,
            unstaged: false,
            commits: vec![Sha::from_hex("0123456789012345678901234567890123456789").unwrap()],
            removed: None,
        });
        assert_eq!(
            value,
            json!({
                "project": "tide",
                "workspace": "root",
                "source": "work",
                "squash": false,
                "unstaged": false,
                "commits": ["0123456789012345678901234567890123456789"],
                "removed": null,
            }),
        );
    }

    #[test]
    /// A merge that removes its source reports the removal, its branches, the children it moved,
    /// and the session it killed.
    fn integration_machine_merge_reports_its_removal() {
        let value = to_value(&machine::MergeJson {
            project: "tide",
            workspace: "root",
            source: "work",
            squash: true,
            unstaged: true,
            commits: Vec::new(),
            removed: Some(machine::MergeRemovedJson {
                workspace: "work".to_string(),
                path: "/p/tide/work".to_string(),
                branch: Some("work".to_string()),
                deleted_branch: Some("work".to_string()),
                reparented: vec![machine::OwnedReparentJson {
                    workspace: "child".to_string(),
                    parent: Some("root".to_string()),
                }],
            }),
        });
        assert_eq!(value["removed"]["workspace"], "work");
        assert_eq!(value["removed"]["deleted_branch"], "work");
        assert_eq!(value["removed"]["reparented"][0]["parent"], "root");
    }

    #[test]
    fn integration_machine_recovery_names_its_operation_and_outcome() {
        assert_eq!(
            to_value(&machine::IntegrationJson {
                project: "tide",
                workspace: "work",
                operation: "merge",
                outcome: "continued",
                head: "a".repeat(40),
            }),
            json!({
                "project": "tide",
                "workspace": "work",
                "operation": "merge",
                "outcome": "continued",
                "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            }),
        );
    }

    #[test]
    fn integration_machine_rebase_names_its_outcome() {
        let value = to_value(&machine::RebaseJson {
            project: "tide",
            workspace: "work",
            base: "root",
            base_head: "a".repeat(40),
            previous_head: "b".repeat(40),
            all: false,
            outcome: "reanchored",
        });
        assert_eq!(
            value,
            json!({
                "project": "tide",
                "workspace": "work",
                "base": "root",
                "base_head": "a".repeat(40),
                "previous_head": "b".repeat(40),
                "all": false,
                "outcome": "reanchored",
            }),
        );
    }

    #[test]
    /// A stopped replay is a result, not a failure, so it keeps the success shape.
    fn integration_machine_rebase_reports_a_stopped_replay() {
        let value = to_value(&machine::RebaseJson {
            project: "tide",
            workspace: "work",
            base: "root",
            base_head: "a".repeat(40),
            previous_head: "b".repeat(40),
            all: true,
            outcome: "stopped_mid_rebase",
        });
        assert_eq!(value["outcome"], "stopped_mid_rebase");
        assert_eq!(value["all"], true);
    }

    #[test]
    /// Create and list share one snapshot shape.
    fn integration_machine_snapshot_names_its_archive() {
        let value = to_value(&machine::SnapshotJson {
            workspace: "work",
            sha: &"a".repeat(40),
            path: Path::new("/p/.subspace/snapshots/work-aaaaaaaa.tar.gz"),
            bytes: 42,
        });
        assert_eq!(
            value,
            json!({
                "workspace": "work",
                "sha": "a".repeat(40),
                "path": "/p/.subspace/snapshots/work-aaaaaaaa.tar.gz",
                "bytes": 42,
            }),
        );
    }

    #[test]
    fn json_channel_usage_error_writes_a_json_document_in_json_mode() {
        let args: Vec<String> = ["subspace", "--json", "--unknown-flag"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(wants_json_usage_error(&args));
        let err = match Cli::try_parse_from(&args) {
            Ok(_) => panic!("expected a usage error"),
            Err(e) => e,
        };
        let mut channel = output::Channel::new(
            Vec::<u8>::new(),
            Vec::<u8>::new(),
            Vec::<u8>::new(),
            true,
            Verbosity::Normal,
        );
        let status = report_usage_error(&mut channel, &err);
        assert_ne!(status, 0);
        let machine = String::from_utf8(channel.into_machine()).unwrap();
        let value: serde_json::Value = serde_json::from_str(machine.trim()).unwrap();
        assert_eq!(value["error"]["code"], "usage");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--unknown-flag")
        );
    }

    #[test]
    fn json_channel_usage_error_stays_clap_text_in_text_mode() {
        let args: Vec<String> = ["subspace", "--unknown-flag"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(!wants_json_usage_error(&args));
        let err = match Cli::try_parse_from(&args) {
            Ok(_) => panic!("expected a usage error"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(err.to_string().contains("--unknown-flag"));
    }

    #[test]
    fn json_channel_help_does_not_produce_an_error_document() {
        let args: Vec<String> = ["subspace", "--json", "--help"]
            .into_iter()
            .map(String::from)
            .collect();
        let err = match Cli::try_parse_from(&args) {
            Ok(_) => panic!("expected clap to intercept --help"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn json_channel_flag_is_global_and_off_by_default() {
        assert!(!Cli::parse_from(["subspace", "current"]).json);
        assert!(Cli::parse_from(["subspace", "--json", "current"]).json);
        assert!(Cli::parse_from(["subspace", "current", "--json"]).json);
    }

    #[test]
    fn json_channel_gate_names_the_commands_with_a_machine_result() {
        assert!(json_supported(
            &Cli::parse_from(["subspace", "current"]).command
        ));
        assert!(!json_supported(
            &Cli::parse_from(["subspace", "rebase", "-i"]).command
        ));
    }

    #[test]
    fn json_channel_gate_covers_show_project_list_and_snapshot_list() {
        assert!(json_supported(
            &Cli::parse_from(["subspace", "show"]).command
        ));
        assert!(json_supported(
            &Cli::parse_from(["subspace", "project", "list"]).command
        ));
        assert!(json_supported(
            &Cli::parse_from(["subspace", "snapshot", "list"]).command
        ));
    }

    #[test]
    fn show_global_json_flag_is_accepted_in_the_old_local_placement() {
        let cli = Cli::parse_from(["subspace", "show", "--json"]);
        assert!(cli.json);
        assert!(matches!(cli.command, Cmd::Show));
    }

    #[test]
    fn show_global_json_flag_is_accepted_before_the_subcommand() {
        let cli = Cli::parse_from(["subspace", "--json", "show"]);
        assert!(cli.json);
        assert!(matches!(cli.command, Cmd::Show));
    }

    /// The old local placement still parses.
    /// Clap resolves `--json` here to the global flag now, not to a removed action-local one.
    #[test]
    fn project_list_and_snapshot_list_take_only_the_global_json_flag() {
        let old_placement = Cli::parse_from(["subspace", "project", "list", "--json"]);
        assert!(old_placement.json);
        assert!(matches!(
            old_placement.command,
            Cmd::Project {
                action: ProjectAction::List
            }
        ));
        assert!(Cli::parse_from(["subspace", "--json", "project", "list"]).json);
        assert!(Cli::parse_from(["subspace", "snapshot", "list", "--json"]).json);
        assert!(Cli::parse_from(["subspace", "--json", "snapshot", "list"]).json);
    }

    #[test]
    fn json_channel_keeps_the_color_flags() {
        let color = |argv: &[&str]| Cli::parse_from(argv).settings(false, false).color;
        assert!(color(&[
            "subspace", "--json", "--color", "always", "current"
        ]));
        assert!(!color(&[
            "subspace", "--json", "--color", "never", "current"
        ]));
        assert!(color(&["subspace", "--color", "always", "current"]));
    }

    /// `--json` uses stderr for automatic color, never stdout, and text mode uses stdout, never
    /// stderr. Both fixtures below flip stdout and stderr against each other, so a swap between the
    /// two streams fails the assertion even when the test process has no terminal on either stream.
    #[test]
    fn json_channel_auto_color_reads_stderr_instead_of_stdout() {
        let color = |argv: &[&str], stdout_terminal, stderr_terminal| {
            Cli::parse_from(argv)
                .settings(stdout_terminal, stderr_terminal)
                .color
        };
        for (stdout_terminal, stderr_terminal) in [(true, false), (false, true)] {
            assert_eq!(
                color(
                    &["subspace", "--json", "current"],
                    stdout_terminal,
                    stderr_terminal
                ),
                stderr_terminal
            );
            assert_eq!(
                color(&["subspace", "current"], stdout_terminal, stderr_terminal),
                stdout_terminal
            );
        }
    }

    #[test]
    fn json_channel_keeps_the_verbosity_flags() {
        let of = |argv: &[&str]| Cli::parse_from(argv).verbosity();
        assert_eq!(of(&["subspace", "--json", "current"]), Verbosity::Normal);
        assert_eq!(
            of(&["subspace", "--json", "-q", "current"]),
            Verbosity::Quiet
        );
        assert_eq!(
            of(&["subspace", "--json", "-v", "current"]),
            Verbosity::Verbose
        );
    }

    #[test]
    fn json_channel_keeps_the_interactive_flags() {
        let of = |argv: &[&str]| Cli::parse_from(argv).settings(false, false).interactive;
        assert!(of(&["subspace", "--json", "--interactive", "current"]));
        assert!(!of(&["subspace", "--json", "--no-interactive", "current"]));
    }

    /// A workspace status with one of every dimension set: a rewritten parent with a replay base,
    /// a remote, an in-progress rebase, and one file in each state.
    fn full_info() -> WorkspaceInfo {
        use subspace::worktree::RebaseSteps;
        WorkspaceInfo {
            git_status: subspace::git::GitStatus {
                index: vec![
                    (PathBuf::from("a"), GitFileStatus::Added),
                    (PathBuf::from("b"), GitFileStatus::Modified),
                    (PathBuf::from("c"), GitFileStatus::Renamed),
                ],
                worktree: vec![
                    (PathBuf::from("d"), GitFileStatus::Deleted),
                    (PathBuf::from("e"), GitFileStatus::Untracked),
                    (PathBuf::from("f"), GitFileStatus::Unmerged),
                ],
            },
            parent: Some(subspace::workspace::ParentData {
                name: "root".to_string(),
                commits_behind: 2,
                commits_ahead: 3,
                rewritten: true,
                replay_base: Some(
                    Sha::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            }),
            remote: Some(worktree::UpstreamStatus {
                ahead: 4,
                behind: 5,
            }),
            operation: Some(InProgressOperation::Rebase {
                steps: Some(RebaseSteps { done: 1, total: 7 }),
            }),
        }
    }

    /// Project metadata that gives `child` a branch and a parent edge to `root`.
    fn two_workspace_meta() -> ProjectMeta {
        let mut meta = ProjectMeta::default();
        meta.workspaces.insert(
            "root".to_string(),
            WorkspaceMeta {
                parent: None,
                branch: Some("main".to_string()),
                integration: None,
            },
        );
        meta.workspaces.insert(
            "child".to_string(),
            WorkspaceMeta {
                parent: Some(subspace::metadata::ParentEdge {
                    name: "root".to_string(),
                    base: Sha::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                }),
                branch: Some("work/child".to_string()),
                integration: None,
            },
        );
        meta
    }

    fn to_value(value: &impl Serialize) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }

    #[test]
    fn context_machine_gate_covers_every_context_command() {
        for argv in [
            vec!["subspace", "current"],
            vec!["subspace", "path"],
            vec!["subspace", "root"],
            vec!["subspace", "parent"],
            vec!["subspace", "children"],
            vec!["subspace", "siblings"],
            vec!["subspace", "list"],
            vec!["subspace", "list", "--parents"],
            vec!["subspace", "show"],
            vec!["subspace", "tree"],
        ] {
            assert!(
                json_supported(&Cli::parse_from(&argv).command),
                "{argv:?} has no machine result"
            );
        }
    }

    #[test]
    fn context_machine_name_and_path_results_are_named_objects() {
        assert_eq!(
            to_value(&machine::NameJson { name: "child" }),
            json!({"name": "child"})
        );
        assert_eq!(
            to_value(&machine::PathJson {
                path: "/w/child".to_string(),
                scope: "workspace",
            }),
            json!({"path": "/w/child", "scope": "workspace"}),
        );
    }

    #[test]
    fn context_machine_current_carries_the_project_and_the_bare_name() {
        assert_eq!(
            to_value(&machine::CurrentJson {
                project: "tide",
                name: "child",
            }),
            json!({"project": "tide", "name": "child"}),
        );
    }

    #[test]
    fn context_machine_show_project_scope_keeps_one_entry_per_workspace() {
        let info = full_info();
        let meta = two_workspace_meta();
        assert_eq!(
            to_value(&machine::StatusEntryJson {
                name: "child",
                status: Some(machine::workspace_json(&info, &meta, "child")),
            })["status"]["name"],
            "child",
        );
        assert_eq!(
            to_value(&machine::StatusEntryJson {
                name: "unreadable",
                status: None,
            }),
            json!({"name": "unreadable", "status": null}),
        );
    }

    #[test]
    fn context_machine_list_always_names_the_parent_edge() {
        let entries = vec![
            machine::ListEntryJson {
                name: "root",
                parent: None,
            },
            machine::ListEntryJson {
                name: "child",
                parent: Some("root"),
            },
        ];
        assert_eq!(
            to_value(&entries),
            json!([
                {"name": "root", "parent": null},
                {"name": "child", "parent": "root"},
            ]),
        );
    }

    #[test]
    /// `--parents` only changes the text form.
    fn context_machine_list_entries_json_names_each_parent_edge() {
        let mut edges = HashMap::new();
        edges.insert("child".to_string(), "root".to_string());
        let names = ["root", "child"];

        let entries = machine::list_entries_json(&names, &edges);
        assert_eq!(
            to_value(&entries),
            json!([
                {"name": "root", "parent": null},
                {"name": "child", "parent": "root"},
            ]),
        );
    }

    #[test]
    fn context_machine_status_carries_parent_remote_operation_and_files() {
        let info = full_info();
        let meta = two_workspace_meta();
        assert_eq!(
            to_value(&machine::workspace_json(&info, &meta, "child")),
            json!({
                "name": "child",
                "branch": "work/child",
                "parent": {
                    "name": "root",
                    "commits_behind": 2,
                    "commits_ahead": 3,
                    "rewritten": true,
                    "replay_base": "0123456789abcdef0123456789abcdef01234567",
                },
                "remote": {"ahead": 4, "behind": 5},
                "operation": {"kind": "rebase", "done": 1, "total": 7},
                "files": {
                    "added": 1,
                    "modified": 1,
                    "renamed": 1,
                    "deleted": 1,
                    "untracked": 1,
                    "unmerged": 1,
                    "staged": [
                        {"path": "a", "status": "added"},
                        {"path": "b", "status": "modified"},
                        {"path": "c", "status": "renamed"},
                    ],
                    "unstaged": [
                        {"path": "d", "status": "deleted"},
                        {"path": "e", "status": "untracked"},
                        {"path": "f", "status": "unmerged"},
                    ],
                },
            }),
        );
    }

    #[test]
    fn context_machine_status_leaves_absent_dimensions_null() {
        let info = WorkspaceInfo {
            git_status: subspace::git::GitStatus::default(),
            parent: None,
            remote: None,
            operation: None,
        };
        let value = to_value(&machine::workspace_json(
            &info,
            &ProjectMeta::default(),
            "root",
        ));
        assert_eq!(value["branch"], serde_json::Value::Null);
        assert_eq!(value["parent"], serde_json::Value::Null);
        assert_eq!(value["remote"], serde_json::Value::Null);
        assert_eq!(value["operation"], serde_json::Value::Null);
        assert_eq!(value["files"]["added"], 0);
        assert_eq!(value["files"]["staged"], json!([]));
        assert_eq!(value["files"]["unstaged"], json!([]));
    }

    #[test]
    fn context_machine_tree_nests_children_in_name_order() {
        let meta = two_workspace_meta();
        let mut info_map = HashMap::new();
        info_map.insert("child".to_string(), full_info());
        let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
        children_of.insert("root", vec!["zeta", "child"]);
        let node = machine::tree_node_json("root", &children_of, &info_map, &meta, Some("child"));
        let value = to_value(&node);
        assert_eq!(value["name"], "root");
        assert_eq!(value["current"], false);
        assert_eq!(value["status"], serde_json::Value::Null);
        let children = value["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0]["name"], "child");
        assert_eq!(children[0]["current"], true);
        assert_eq!(children[0]["status"]["parent"]["name"], "root");
        assert_eq!(children[1]["name"], "zeta");
        assert!(children[1]["children"].as_array().unwrap().is_empty());
    }

    #[test]
    fn context_machine_tree_nodes_json_over_a_temporary_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let worktrees = project.worktrees_path();
        std::fs::create_dir_all(&worktrees).unwrap();

        let root_path = worktrees.join("root");
        std::fs::create_dir(&root_path).unwrap();
        git(&root_path, &["init", "-q"]);
        git(
            &root_path,
            &["commit", "-q", "--allow-empty", "-m", "initial"],
        );
        let fork_output = process::Command::new("git")
            .arg("-C")
            .arg(&root_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let fork = String::from_utf8(fork_output.stdout)
            .unwrap()
            .trim()
            .to_string();

        let child_path = worktrees.join("child");
        git(
            &root_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "child",
                child_path.to_str().unwrap(),
                "HEAD",
            ],
        );

        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                meta.workspaces
                    .insert("root".to_string(), WorkspaceMeta::default());
                meta.workspaces.insert(
                    "child".to_string(),
                    WorkspaceMeta {
                        parent: Some(subspace::metadata::ParentEdge {
                            name: "root".to_string(),
                            base: Sha::from_hex(&fork).unwrap(),
                        }),
                        branch: None,
                        integration: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        drop(lock);

        let meta = project.metadata().read().unwrap();
        let trees = WorkspaceTree::roots(&project).unwrap();
        let mut info_map = HashMap::new();
        for tree in &trees {
            for node in tree.nodes() {
                if let Ok(info) = node.info(&meta) {
                    info_map.insert(node.name().to_string(), info);
                }
            }
        }

        let nodes = machine::tree_nodes_json(&trees, &info_map, &meta, Some("child"));
        let value = to_value(&nodes);
        let roots = value.as_array().unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0]["name"], "root");
        assert_eq!(roots[0]["current"], false);
        let children = roots[0]["children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "child");
        assert_eq!(children[0]["current"], true);
        assert_eq!(children[0]["status"]["parent"]["name"], "root");
    }

    #[test]
    fn workspace_machine_gate_covers_every_workspace_lifecycle_command() {
        for argv in [
            vec!["subspace", "add", "work"],
            vec!["subspace", "add", "work", "--parent", "root"],
            vec!["subspace", "add", "work", "--parent-of", "child"],
            vec!["subspace", "branch", "work"],
            vec!["subspace", "remove", "work"],
            vec!["subspace", "remove", "work", "--keep-branch", "--recursive"],
            vec!["subspace", "rename", "work", "next"],
            vec!["subspace", "repair"],
            vec!["subspace", "repair", "--dry-run"],
            vec!["subspace", "repair", "tide", "--dry-run"],
        ] {
            assert!(
                json_supported(&Cli::parse_from(&argv).command),
                "{argv:?} has no machine result"
            );
        }
    }

    #[test]
    /// A declined removal prompt writes a `declined` failure document and exits with status 1.
    fn workspace_machine_remove_reports_a_declined_prompt() {
        let mut buffer = Vec::new();
        {
            let mut channel = output::Channel::new(
                Vec::new(),
                Vec::new(),
                &mut buffer,
                true,
                subspace::client::Verbosity::Normal,
            );
            let stopped = report_declined(false, "tide/work was not removed", &mut channel);
            assert!(stopped);
            assert_eq!(channel.finish(), 1);
        }
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(buffer).unwrap().trim()).unwrap();
        assert_eq!(value["error"]["code"], "declined");
        assert_eq!(value["error"]["message"], "tide/work was not removed");
    }

    #[test]
    fn workspace_machine_add_names_the_workspace_the_branch_and_the_parent() {
        assert_eq!(
            to_value(&machine::WorkspaceCreatedJson {
                project: "tide",
                workspace: "work",
                path: "/p/tide/work".to_string(),
                branch: Some("work"),
                parent: Some("root"),
                reparented: Vec::new(),
            }),
            json!({
                "project": "tide",
                "workspace": "work",
                "path": "/p/tide/work",
                "branch": "work",
                "parent": "root",
                "reparented": [],
            }),
        );
    }

    #[test]
    /// A detached root workspace has no branch and no parent, and both stay null.
    fn workspace_machine_add_leaves_a_detached_root_null() {
        let value = to_value(&machine::WorkspaceCreatedJson {
            project: "tide",
            workspace: "work",
            path: "/p/tide/work".to_string(),
            branch: None,
            parent: None,
            reparented: Vec::new(),
        });
        assert_eq!(value["branch"], json!(null));
        assert_eq!(value["parent"], json!(null));
    }

    #[test]
    /// `--parent-of` moves the child onto the new workspace, so the result names that change.
    fn workspace_machine_add_names_the_reparented_child() {
        let value = to_value(&machine::WorkspaceCreatedJson {
            project: "tide",
            workspace: "mid",
            path: "/p/tide/mid".to_string(),
            branch: Some("mid"),
            parent: Some("root"),
            reparented: vec![machine::ReparentJson {
                workspace: "child",
                parent: Some("mid"),
            }],
        });
        assert_eq!(
            value["reparented"],
            json!([{"workspace": "child", "parent": "mid"}]),
        );
    }

    #[test]
    /// A recursive removal names every deleted workspace, each new parent, and each killed session.
    fn workspace_machine_remove_lists_every_removed_resource() {
        let value = to_value(&machine::WorkspaceRemovedJson {
            project: "tide",
            removed: vec![
                machine::RemovedWorkspaceJson {
                    workspace: "child".to_string(),
                    path: "/p/tide/child".to_string(),
                    branch: Some("child".to_string()),
                    deleted_branch: Some("child".to_string()),
                },
                machine::RemovedWorkspaceJson {
                    workspace: "work".to_string(),
                    path: "/p/tide/work".to_string(),
                    branch: None,
                    deleted_branch: None,
                },
            ],
            reparented: vec![machine::ReparentJson {
                workspace: "other",
                parent: None,
            }],
        });
        assert_eq!(
            value,
            json!({
                "project": "tide",
                "removed": [
                    {
                        "workspace": "child",
                        "path": "/p/tide/child",
                        "branch": "child",
                        "deleted_branch": "child",
                    },
                    {
                        "workspace": "work",
                        "path": "/p/tide/work",
                        "branch": null,
                        "deleted_branch": null,
                    },
                ],
                "reparented": [{"workspace": "other", "parent": null}],
            }),
        );
    }

    #[test]
    fn workspace_machine_rename_names_both_names_and_both_paths() {
        assert_eq!(
            to_value(&machine::WorkspaceRenameJson {
                project: "tide",
                workspace: "next",
                previous: "work",
                path: "/p/tide/next".to_string(),
                previous_path: "/p/tide/work".to_string(),
            }),
            json!({
                "project": "tide",
                "workspace": "next",
                "previous": "work",
                "path": "/p/tide/next",
                "previous_path": "/p/tide/work",
            }),
        );
    }

    #[test]
    /// Each unrepairable reason keeps one stable code, and only the branch reasons carry a branch.
    fn workspace_machine_repair_gives_each_unrepairable_reason_a_stable_code() {
        let cases = [
            (Unrepairable::NotAWorktree, "not_a_worktree"),
            (Unrepairable::NoBranch, "no_branch"),
            (
                Unrepairable::MissingBranch("work".to_string()),
                "missing_branch",
            ),
            (
                Unrepairable::BranchInUse {
                    branch: "work".to_string(),
                    holder: "/p/tide/other".to_string(),
                },
                "branch_in_use",
            ),
            (
                Unrepairable::RebuildFailed("boom".to_string()),
                "rebuild_failed",
            ),
        ];
        for (why, code) in &cases {
            let value = to_value(&machine::unrepairable_json("work", why));
            assert_eq!(value["workspace"], "work");
            assert_eq!(value["reason"], *code);
            assert!(!value["message"].as_str().unwrap().is_empty());
        }
        let in_use = to_value(&machine::unrepairable_json("work", &cases[3].0));
        assert_eq!(in_use["branch"], "work");
        assert_eq!(in_use["holder"], "/p/tide/other");
        let no_branch = to_value(&machine::unrepairable_json("work", &cases[1].0));
        assert_eq!(no_branch["branch"], json!(null));
        assert_eq!(no_branch["holder"], json!(null));
    }

    #[test]
    /// The repair result records the mode, whether the link half ran, and every repair record.
    fn workspace_machine_repair_records_both_modes_and_every_record() {
        let value = to_value(&machine::RepairJson {
            project: "tide",
            dry_run: true,
            links_checked: true,
            relinked: vec![machine::RelinkJson {
                workspace: "work",
                branch: "work",
            }],
            unrepairable: vec![machine::unrepairable_json("other", &Unrepairable::NoBranch)],
            pruned: vec!["dead"],
            reparented: vec![machine::ReparentJson {
                workspace: "child",
                parent: Some("root"),
            }],
            cleared_default: Some("dead"),
        });
        assert_eq!(value["project"], "tide");
        assert_eq!(value["dry_run"], true);
        assert_eq!(value["links_checked"], true);
        assert_eq!(
            value["relinked"],
            json!([{"workspace": "work", "branch": "work"}]),
        );
        assert_eq!(value["unrepairable"][0]["reason"], "no_branch");
        assert_eq!(value["pruned"], json!(["dead"]));
        assert_eq!(
            value["reparented"],
            json!([{"workspace": "child", "parent": "root"}]),
        );
        assert_eq!(value["cleared_default"], "dead");
    }

    #[test]
    /// A clean repair pass that could not open the repository reports empty records and says that
    /// the link half did not run.
    fn workspace_machine_repair_says_when_the_link_half_did_not_run() {
        let value = to_value(&machine::RepairJson {
            project: "tide",
            dry_run: false,
            links_checked: false,
            relinked: Vec::new(),
            unrepairable: Vec::new(),
            pruned: Vec::new(),
            reparented: Vec::new(),
            cleared_default: None,
        });
        assert_eq!(
            value,
            json!({
                "project": "tide",
                "dry_run": false,
                "links_checked": false,
                "relinked": [],
                "unrepairable": [],
                "pruned": [],
                "reparented": [],
                "cleared_default": null,
            }),
        );
    }

    #[test]
    fn machine_gate_covers_the_setting_reads_and_writes() {
        for argv in [
            vec!["subspace", "default"],
            vec!["subspace", "default", "--set", "child"],
            vec!["subspace", "branch-prefix"],
            vec!["subspace", "branch-prefix", "--set", "work/"],
            vec!["subspace", "branch-prefix", "--unset"],
            vec!["subspace", "workspace-branch"],
            vec!["subspace", "workspace-branch", "--set", "work"],
            vec!["subspace", "workspace-branch", "--unset"],
        ] {
            assert!(
                json_supported(&Cli::parse_from(&argv).command),
                "{argv:?} has no machine result"
            );
        }
    }

    #[test]
    fn settings_read_without_a_flag_and_write_with_set() {
        for command in ["default", "branch-prefix", "workspace-branch"] {
            assert!(Cli::try_parse_from(["subspace", command]).is_ok());
            assert!(Cli::try_parse_from(["subspace", command, "--set", "work"]).is_ok());
            assert!(Cli::try_parse_from(["subspace", command, "--set"]).is_err());
            assert!(Cli::try_parse_from(["subspace", command, "--set", ""]).is_err());
            assert!(Cli::try_parse_from(["subspace", command, "work"]).is_err());
        }
        for command in ["branch-prefix", "workspace-branch"] {
            assert!(Cli::try_parse_from(["subspace", command, "--unset"]).is_ok());
            assert!(
                Cli::try_parse_from(["subspace", command, "--set", "work", "--unset"]).is_err()
            );
        }
    }

    #[test]
    fn machine_rejected_action_gives_the_unsupported_error_document() {
        let cli = Cli::parse_from(["subspace", "rebase", "-i"]);
        assert!(!json_supported(&cli.command));
        let mut buffer = Vec::new();
        {
            let mut channel = output::Channel::new(
                Vec::new(),
                Vec::new(),
                &mut buffer,
                true,
                subspace::client::Verbosity::Normal,
            );
            channel.failure("unsupported", "the command has no JSON result");
            assert_eq!(channel.finish(), 1);
        }
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(buffer).unwrap().trim()).unwrap();
        assert_eq!(value["error"]["code"], "unsupported");
    }

    #[test]
    fn machine_setting_results_name_the_project_and_the_value() {
        assert_eq!(
            to_value(&machine::SettingJson {
                project: "tide",
                setting: "default",
                value: Some("root"),
            }),
            json!({"project": "tide", "setting": "default", "value": "root"}),
        );
        assert_eq!(
            to_value(&machine::SettingJson {
                project: "tide",
                setting: "branch_prefix",
                value: Some("work/"),
            }),
            json!({"project": "tide", "setting": "branch_prefix", "value": "work/"}),
        );
    }

    #[test]
    /// An unset prefix, and a removal, both leave the value null.
    fn machine_an_absent_setting_leaves_the_value_null() {
        assert_eq!(
            to_value(&machine::SettingJson {
                project: "tide",
                setting: "branch_prefix",
                value: None,
            }),
            json!({"project": "tide", "setting": "branch_prefix", "value": null}),
        );
    }

    #[test]
    /// Project mutation previews resolve without a current project or workspace.
    fn project_mutation_preview_works_outside_a_registered_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_at(dir.path());
        let source = project_root_with_a_worktree(dir.path(), "source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&destination).unwrap();
        registry
            .update(|state| state.insert("source", source.clone()))
            .unwrap();
        let state = registry.read().unwrap();
        let init_path = destination.join("created");
        let commands = [
            vec![
                "subspace",
                "project",
                "clone",
                "origin/repo",
                destination.to_str().unwrap(),
            ],
            vec!["subspace", "project", "init", init_path.to_str().unwrap()],
            vec!["subspace", "project", "remove", "source"],
            vec![
                "subspace",
                "project",
                "rename",
                "source",
                "renamed",
                "--no-move",
            ],
            vec![
                "subspace",
                "project",
                "move",
                "source",
                destination.to_str().unwrap(),
            ],
        ];
        for argv in commands {
            let cli = Cli::parse_from(argv);
            let action = mutation_action(&cli.command).unwrap();
            mutation_effects(&state, &cli, action).unwrap();
        }
    }

    #[test]
    fn project_machine_gate_covers_every_project_lifecycle_command() {
        for argv in [
            vec!["subspace", "project", "current"],
            vec!["subspace", "project", "list"],
            vec!["subspace", "project", "remove", "tide"],
            vec![
                "subspace",
                "project",
                "remove",
                "tide",
                "--remove-directory",
            ],
            vec!["subspace", "project", "rename", "tide", "flow"],
            vec!["subspace", "project", "move", "tide", "/p"],
            vec!["subspace", "project", "clone", "git@host:tide.git"],
            vec!["subspace", "project", "init", "/p/tide"],
            vec!["subspace", "project", "init", "/p/tide", "--name", "flow"],
        ] {
            assert!(
                json_supported(&Cli::parse_from(&argv).command),
                "{argv:?} has no machine result"
            );
        }
    }

    #[test]
    fn project_machine_current_names_the_project_and_the_path() {
        assert_eq!(
            to_value(&machine::ProjectJson {
                project: "tide",
                path: "/p/tide".to_string(),
            }),
            json!({"project": "tide", "path": "/p/tide"}),
        );
    }

    #[test]
    /// A declined removal writes a `declined` failure document and exits with status 1.
    fn project_machine_remove_reports_a_declined_prompt() {
        let mut buffer = Vec::new();
        {
            let mut channel = output::Channel::new(
                Vec::new(),
                Vec::new(),
                &mut buffer,
                true,
                subspace::client::Verbosity::Normal,
            );
            let stopped = report_declined(false, "tide was not deregistered", &mut channel);
            assert!(stopped);
            assert_eq!(channel.finish(), 1);
        }
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(buffer).unwrap().trim()).unwrap();
        assert_eq!(value["error"]["code"], "declined");
        assert!(value["error"]["message"].as_str().unwrap().contains("tide"));
    }

    #[test]
    /// A plain deregistration deletes nothing, so its removal list is empty.
    fn project_machine_remove_lists_every_deleted_resource() {
        assert_eq!(
            to_value(&machine::ProjectRemovedJson {
                project: "tide",
                path: "/p/tide".to_string(),
                removed: Vec::new(),
            }),
            json!({"project": "tide", "path": "/p/tide", "removed": []}),
        );
        assert_eq!(
            to_value(&machine::ProjectRemovedJson {
                project: "tide",
                path: "/p/tide".to_string(),
                removed: vec![
                    "/p/tide".to_string(),
                    "/c/tide".to_string(),
                    "/s/tide".to_string(),
                ],
            }),
            json!({
                "project": "tide",
                "path": "/p/tide",
                "removed": ["/p/tide", "/c/tide", "/s/tide"],
            }),
        );
    }

    #[test]
    fn project_rename_moves_only_subspace_config_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let old_config = dir.path().join("config/projects/tide");
        let new_config = dir.path().join("config/projects/flow");
        let old_state = dir.path().join("state/projects/tide");
        let new_state = dir.path().join("state/projects/flow");
        std::fs::create_dir_all(&old_config).unwrap();
        std::fs::create_dir_all(&new_config).unwrap();
        std::fs::create_dir_all(&old_state).unwrap();
        std::fs::write(old_config.join("subspace.json"), "{}").unwrap();
        std::fs::write(old_config.join("env.json"), "old env").unwrap();
        std::fs::write(old_config.join("manifest.json"), "old manifest").unwrap();
        std::fs::write(new_config.join("env.json"), "new env").unwrap();
        std::fs::write(new_config.join("manifest.json"), "new manifest").unwrap();
        std::fs::write(old_state.join("workspaces.json"), "{}").unwrap();

        let undo = relocate_project_metadata(
            &old_config.join("subspace.json"),
            &new_config.join("subspace.json"),
            &old_state,
            &new_state,
        )
        .unwrap();

        assert!(!old_config.join("subspace.json").exists());
        assert!(old_config.join("env.json").exists());
        assert!(old_config.join("manifest.json").exists());
        assert!(new_config.join("subspace.json").exists());
        assert_eq!(
            std::fs::read_to_string(new_config.join("env.json")).unwrap(),
            "new env"
        );
        assert_eq!(
            std::fs::read_to_string(new_config.join("manifest.json")).unwrap(),
            "new manifest"
        );
        assert!(new_state.join("workspaces.json").exists());

        undo().unwrap();

        assert!(old_config.join("subspace.json").exists());
        assert!(old_state.join("workspaces.json").exists());
        assert!(!new_config.join("subspace.json").exists());
        assert!(!new_state.exists());
    }

    #[test]
    fn project_machine_rename_names_both_names_and_the_directory_move() {
        assert_eq!(
            to_value(&machine::ProjectRenameJson {
                project: "flow",
                previous: "tide",
                path: "/p/flow".to_string(),
                previous_path: "/p/tide".to_string(),
            }),
            json!({
                "project": "flow",
                "previous": "tide",
                "path": "/p/flow",
                "previous_path": "/p/tide",
            }),
        );
        assert_eq!(
            to_value(&machine::ProjectRenameJson {
                project: "flow",
                previous: "tide",
                path: "/p/tide".to_string(),
                previous_path: "/p/tide".to_string(),
            })["previous_path"],
            json!("/p/tide"),
        );
    }

    #[test]
    fn project_machine_move_names_both_directories() {
        assert_eq!(
            to_value(&machine::ProjectMoveJson {
                project: "tide",
                path: "/new/tide".to_string(),
                previous_path: "/old/tide".to_string(),
            }),
            json!({
                "project": "tide",
                "path": "/new/tide",
                "previous_path": "/old/tide",
            }),
        );
    }

    #[test]
    fn project_machine_creation_names_the_project_and_its_root_workspace() {
        assert_eq!(
            to_value(&machine::ProjectCreatedJson {
                project: "tide",
                path: "/p/tide".to_string(),
                workspace: "main",
            }),
            json!({"project": "tide", "path": "/p/tide", "workspace": "main"}),
        );
    }

    #[test]
    /// The list keeps its map of project name to project directory.
    fn project_machine_list_keeps_the_name_to_path_map() {
        let map: std::collections::BTreeMap<&str, String> = [
            ("flow", "/p/flow".to_string()),
            ("tide", "/p/tide".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            to_value(&map),
            json!({"flow": "/p/flow", "tide": "/p/tide"}),
        );
    }

    /// Builds a project root with one git worktree under
    /// it.
    fn project_root_with_a_worktree(dir: &Path, name: &str) -> PathBuf {
        let root = dir.join(name);
        let work = root.join("worktrees").join("root");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q"]);
        root
    }

    /// Builds a registry whose every document sits under `dir`, so a test writes nothing outside
    /// its own temporary directory.
    fn registry_at(dir: &Path) -> Registry {
        Registry::new(
            dir.join("state").join("projects.json"),
            dir.join("state").join("flat.json"),
            dir.join("config").join("config.json"),
            dir.join("legacy"),
            dir.join("legacy.json"),
        )
    }

    /// Builds a runner over `hooks` whose every command fails.
    fn failing_hook_runner(hooks: &str) -> HookRunner {
        let hooks: subspace::hook::Hooks = serde_json::from_str(hooks).unwrap();
        HookRunner::with_exec(
            hooks,
            Box::new(|_command: &str, _env: &[(String, String)]| Err("exit 1".to_string())),
        )
    }

    /// Builds a runner over `hooks` whose every command succeeds.
    fn passing_hook_runner(hooks: &str) -> HookRunner {
        let hooks: subspace::hook::Hooks = serde_json::from_str(hooks).unwrap();
        HookRunner::with_exec(
            hooks,
            Box::new(|_command: &str, _env: &[(String, String)]| Ok(())),
        )
    }

    /// A JSON channel over three buffers.
    fn json_channel() -> output::Channel<Vec<u8>, Vec<u8>, Vec<u8>> {
        output::Channel::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            subspace::client::Verbosity::Normal,
        )
    }

    /// Closes `channel` the way `main` closes a failed command, and returns its one document.
    fn failed_document(
        mut channel: output::Channel<Vec<u8>, Vec<u8>, Vec<u8>>,
        error: anyhow::Error,
    ) -> serde_json::Value {
        channel.failure("failed", &format!("{error:#}"));
        assert_eq!(channel.finish(), 1);
        one_document(channel)
    }

    #[test]
    /// The `_post` hook runs before the success document, so its failure writes the one error document.
    fn project_machine_remove_reports_a_failed_post_hook_as_the_one_document() {
        let dir = tempfile::tempdir().unwrap();
        let root = project_root_with_a_worktree(dir.path(), "tide");
        let registry = registry_at(dir.path());
        registry
            .update(|state| state.insert("tide", root.clone()))
            .unwrap();
        let state = registry.read().unwrap();
        let hooks = failing_hook_runner(r#"{"project_remove_post":"false"}"#);
        let mut channel = json_channel();
        let error = cmd_project_remove(
            &state,
            &registry,
            &hooks,
            ClientSettings {
                color: false,
                interactive: false,
                verbosity: Verbosity::Normal,
            },
            &mut channel,
            "tide",
            false,
        )
        .unwrap_err();
        let value = failed_document(channel, error);
        assert_eq!(value["error"]["code"], "failed");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("project_remove_post"),
            "{}",
            value["error"]["message"]
        );
    }

    #[test]
    /// A `_post` hook that succeeds leaves the success document of `project remove` untouched.
    fn project_machine_remove_writes_the_success_document_after_the_post_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = project_root_with_a_worktree(dir.path(), "tide");
        let registry = registry_at(dir.path());
        registry
            .update(|state| state.insert("tide", root.clone()))
            .unwrap();
        let state = registry.read().unwrap();
        let hooks = passing_hook_runner(r#"{"project_remove_post":"true"}"#);
        let mut channel = json_channel();
        cmd_project_remove(
            &state,
            &registry,
            &hooks,
            ClientSettings {
                color: false,
                interactive: false,
                verbosity: Verbosity::Normal,
            },
            &mut channel,
            "tide",
            false,
        )
        .unwrap();
        assert_eq!(channel.finish(), 0);
        let value = one_document(channel);
        assert_eq!(value["project"], "tide");
        assert_eq!(value["removed"], json!([]));
    }

    #[test]
    fn range_machine_gate_covers_diff_and_log_with_each_range_form() {
        for argv in [
            vec!["subspace", "diff"],
            vec!["subspace", "diff", "--parent"],
            vec!["subspace", "diff", "--root"],
            vec!["subspace", "diff", "--in", "child"],
            vec!["subspace", "diff", "--from", "child"],
            vec!["subspace", "diff", "--to", "child"],
            vec!["subspace", "diff", "child"],
            vec!["subspace", "diff", "a..b"],
            vec!["subspace", "diff", "a...b"],
            vec!["subspace", "log"],
            vec!["subspace", "log", "--parent"],
            vec!["subspace", "log", "--root"],
            vec!["subspace", "log", "--in", "child"],
            vec!["subspace", "log", "--from", "child"],
            vec!["subspace", "log", "--to", "child"],
            vec!["subspace", "log", "child"],
            vec!["subspace", "log", "a..b"],
            vec!["subspace", "log", "a...b"],
        ] {
            assert!(
                json_supported(&Cli::parse_from(&argv).command),
                "{argv:?} has no machine result"
            );
        }
    }

    #[test]
    fn range_machine_split_target_resolves_the_range_for_each_range_form() {
        let dir = tempfile::tempdir().unwrap();

        let project = Project::with_dirs(
            "subspace-range-machine-test".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let worktrees = project.worktrees_path();
        std::fs::create_dir_all(&worktrees).unwrap();

        let root_path = worktrees.join("root");
        std::fs::create_dir(&root_path).unwrap();
        git(&root_path, &["init", "-q"]);
        git(
            &root_path,
            &["commit", "-q", "--allow-empty", "-m", "initial"],
        );
        let fork_output = process::Command::new("git")
            .arg("-C")
            .arg(&root_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let fork = String::from_utf8(fork_output.stdout)
            .unwrap()
            .trim()
            .to_string();

        let child_path = worktrees.join("child");
        git(
            &root_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "child",
                child_path.to_str().unwrap(),
                "HEAD",
            ],
        );

        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                meta.workspaces
                    .insert("root".to_string(), WorkspaceMeta::default());
                meta.workspaces.insert(
                    "child".to_string(),
                    WorkspaceMeta {
                        parent: Some(subspace::metadata::ParentEdge {
                            name: "root".to_string(),
                            base: Sha::from_hex(&fork).unwrap(),
                        }),
                        branch: None,
                        integration: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        drop(lock);

        let mut state = GlobalState::default();
        state
            .insert("subspace-range-machine-test", project.path().to_path_buf())
            .unwrap();

        let child = Workspace::get(&project, "child").unwrap();
        let ctx = Context::for_workspace(child);

        let no_range = || RangeArgs {
            output: None,
            paths: Vec::new(),
            parent: false,
            root: false,
            in_ws: None,
            from: None,
            to: None,
        };
        let expected = format!("{fork}...{fork}");
        let no_flags = Vec::<String>::new();

        let (rev, extra) = split_target(
            &ctx,
            &state,
            &RangeArgs {
                parent: true,
                ..no_range()
            },
            &no_flags,
        )
        .unwrap();
        assert_eq!(rev.as_deref(), Some(expected.as_str()));
        assert!(extra.is_empty());

        let (rev, extra) = split_target(
            &ctx,
            &state,
            &RangeArgs {
                root: true,
                ..no_range()
            },
            &no_flags,
        )
        .unwrap();
        assert_eq!(rev.as_deref(), Some(expected.as_str()));
        assert!(extra.is_empty());

        let (rev, extra) = split_target(
            &ctx,
            &state,
            &RangeArgs {
                in_ws: Some("child".to_string()),
                ..no_range()
            },
            &no_flags,
        )
        .unwrap();
        assert_eq!(rev.as_deref(), Some(expected.as_str()));
        assert!(extra.is_empty());

        let (rev, extra) = split_target(
            &ctx,
            &state,
            &RangeArgs {
                from: Some("child".to_string()),
                ..no_range()
            },
            &no_flags,
        )
        .unwrap();
        assert_eq!(rev.as_deref(), Some(expected.as_str()));
        assert!(extra.is_empty());

        let (rev, extra) = split_target(
            &ctx,
            &state,
            &RangeArgs {
                to: Some("child".to_string()),
                ..no_range()
            },
            &no_flags,
        )
        .unwrap();
        assert_eq!(rev.as_deref(), Some(expected.as_str()));
        assert!(extra.is_empty());

        let workspace_positional = vec!["root".to_string()];
        let (rev, extra) = split_target(&ctx, &state, &no_range(), &workspace_positional).unwrap();
        assert_eq!(rev.as_deref(), Some(expected.as_str()));
        assert!(extra.is_empty());

        let raw_range = vec!["root..child".to_string()];
        let (rev, extra) = split_target(&ctx, &state, &no_range(), &raw_range).unwrap();
        assert_eq!(rev.as_deref(), Some(format!("{fork}..{fork}").as_str()));
        assert!(extra.is_empty());

        let (rev, extra) = split_target(&ctx, &state, &no_range(), &no_flags).unwrap();
        assert_eq!(rev, None);
        assert!(extra.is_empty());
    }

    #[test]
    fn range_machine_result_carries_the_range_the_arguments_and_the_stdout() {
        let args = vec!["--color=never".to_string(), "--stat".to_string()];
        assert_eq!(
            to_value(&machine::GitPassthroughJson {
                command: "diff",
                range: Some("root...child"),
                args: &args,
                stdout: "one line\n",
            }),
            json!({
                "command": "diff",
                "range": "root...child",
                "args": ["--color=never", "--stat"],
                "stdout": "one line\n",
            }),
        );
    }

    #[test]
    fn range_machine_result_leaves_an_absent_range_null() {
        let args: Vec<String> = Vec::new();
        assert_eq!(
            to_value(&machine::GitPassthroughJson {
                command: "log",
                range: None,
                args: &args,
                stdout: "",
            })["range"],
            serde_json::Value::Null,
        );
    }

    #[test]
    fn range_machine_json_mode_resolves_automatic_color_to_never() {
        for argv in [
            vec!["subspace", "--json", "diff", "--parent"],
            vec!["subspace", "--json", "log", "--parent"],
        ] {
            let cli = Cli::parse_from(&argv);
            let out =
                output::Channel::new(Vec::new(), Vec::new(), Vec::new(), true, cli.verbosity());
            let settings = capture_settings(&cli, cli.settings(true, true), &out);
            assert_eq!(git_color_arg(settings), "--color=never", "{argv:?}");
        }
    }

    #[test]
    fn range_machine_an_explicit_color_still_wins_in_json_mode() {
        let cli = Cli::parse_from([
            "subspace", "--json", "--color", "always", "diff", "--parent",
        ]);
        let out = output::Channel::new(Vec::new(), Vec::new(), Vec::new(), true, cli.verbosity());
        let settings = capture_settings(&cli, cli.settings(false, false), &out);
        assert_eq!(git_color_arg(settings), "--color=always");
    }

    #[test]
    fn range_machine_json_mode_resolves_a_trailing_color_auto_to_never() {
        for argv in [
            vec!["subspace", "--json", "diff", "--parent", "--color=auto"],
            vec!["subspace", "--json", "log", "--parent", "--color=auto"],
        ] {
            let (color, rest) = passthrough_mode(&argv, true);
            assert_eq!(color, "--color=never", "{argv:?}");
            assert!(rest.is_empty(), "{argv:?}");
        }
    }

    #[test]
    fn range_machine_text_mode_keeps_the_resolved_terminal_color() {
        let cli = Cli::parse_from(["subspace", "diff", "--parent"]);
        let out = output::Channel::new(Vec::new(), Vec::new(), Vec::new(), false, cli.verbosity());
        let settings = capture_settings(&cli, cli.settings(true, true), &out);
        assert_eq!(git_color_arg(settings), "--color=always");
    }

    #[test]
    fn range_machine_captured_git_reads_stdout_and_a_failure_gives_an_error_document() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        let ok = subspace::worktree::git_capture(
            dir.path(),
            "status",
            None,
            &["--porcelain".to_string()],
        )
        .unwrap();
        assert!(ok.status.success());
        assert_eq!(ok.stdout, "");
        let bad =
            subspace::worktree::git_capture(dir.path(), "log", Some("no-such-ref"), &[]).unwrap();
        assert!(!bad.status.success());
        let mut out: output::Channel<Vec<u8>, Vec<u8>, Vec<u8>> =
            output::Channel::new(Vec::new(), Vec::new(), Vec::new(), true, Verbosity::Normal);
        report_passthrough(&mut out, "log", Some("no-such-ref"), &[], &bad).unwrap();
        assert_eq!(out.finish(), 1);
        let text = String::from_utf8(out.into_machine()).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["error"]["code"], "git");
    }

    struct InspectionFixture {
        _dir: tempfile::TempDir,
        project: Project,
        state: GlobalState,
        root: PathBuf,
        child: PathBuf,
    }

    fn inspection_fixture() -> InspectionFixture {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let worktrees = project.worktrees_path();
        std::fs::create_dir_all(&worktrees).unwrap();
        let root = worktrees.join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let fork = git_head(&root);
        let child = worktrees.join("child");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "child",
                child.to_str().unwrap(),
                "HEAD",
            ],
        );
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                meta.default = Some("child".to_string());
                meta.branch_prefix = Some("topic".to_string());
                meta.workspaces
                    .insert("root".to_string(), WorkspaceMeta::default());
                meta.workspaces.insert(
                    "child".to_string(),
                    WorkspaceMeta {
                        parent: Some(subspace::metadata::ParentEdge {
                            name: "root".to_string(),
                            base: Sha::from_hex(&fork).unwrap(),
                        }),
                        branch: Some("child".to_string()),
                        integration: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        drop(lock);
        let mut state = GlobalState::default();
        state.insert("proj", project.path().to_path_buf()).unwrap();
        InspectionFixture {
            _dir: dir,
            project,
            state,
            root,
            child,
        }
    }

    fn git_head(path: &Path) -> String {
        let output = process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn start_conflicted_rebase(fixture: &InspectionFixture, second_conflict: bool) {
        git(&fixture.child, &["config", "core.editor", "true"]);
        for (path, content) in [("first.txt", "child\n"), ("second.txt", "child\n")] {
            std::fs::write(fixture.child.join(path), content).unwrap();
            git(&fixture.child, &["add", path]);
            git(&fixture.child, &["commit", "-q", "-m", path]);
        }
        std::fs::write(fixture.root.join("first.txt"), "root\n").unwrap();
        git(&fixture.root, &["add", "first.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "first"]);
        if second_conflict {
            std::fs::write(fixture.root.join("second.txt"), "root\n").unwrap();
            git(&fixture.root, &["add", "second.txt"]);
            git(&fixture.root, &["commit", "-q", "-m", "second"]);
        }

        let target = git_head(&fixture.root);
        let status = process::Command::new("git")
            .arg("-C")
            .arg(&fixture.child)
            .args(["rebase", &target])
            .status()
            .unwrap();
        assert!(!status.success());

        let lock = fixture.project.lock("test").unwrap();
        fixture
            .project
            .metadata()
            .update(&lock, |metadata| {
                let entry = metadata.workspaces.get_mut("child").unwrap();
                entry.integration = Some(IntegrationState {
                    original_base: entry.parent.clone(),
                    target: Some(ParentEdge {
                        name: "root".to_string(),
                        base: Sha::from_hex(&target).unwrap(),
                    }),
                    mode: "rebase:interactive=false,all=false".to_string(),
                    squash_message: None,
                });
                Ok(())
            })
            .unwrap();
    }

    fn start_conflicted_merge(fixture: &InspectionFixture) {
        git(&fixture.root, &["config", "core.editor", "true"]);
        std::fs::write(fixture.child.join("merge.txt"), "child\n").unwrap();
        git(&fixture.child, &["add", "merge.txt"]);
        git(&fixture.child, &["commit", "-q", "-m", "child"]);
        std::fs::write(fixture.root.join("merge.txt"), "root\n").unwrap();
        git(&fixture.root, &["add", "merge.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "root"]);

        let status = process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["cherry-pick", &git_head(&fixture.child)])
            .status()
            .unwrap();
        assert!(!status.success());

        let lock = fixture.project.lock("test").unwrap();
        fixture
            .project
            .metadata()
            .update(&lock, |metadata| {
                metadata.workspaces.get_mut("root").unwrap().integration = Some(IntegrationState {
                    original_base: None,
                    target: None,
                    mode: "merge".to_string(),
                    squash_message: None,
                });
                Ok(())
            })
            .unwrap();
    }

    fn integration_document(
        fixture: &InspectionFixture,
        workspace: &str,
        command: Cmd,
    ) -> serde_json::Value {
        let cli = Cli {
            project_name: Some("proj".to_string()),
            project_scope: false,
            workspace_name: Some(workspace.to_string()),
            color: ColorChoice::Never,
            interactive: false,
            no_interactive: true,
            quiet: false,
            verbose: false,
            json: true,
            preview: true,
            command,
        };
        to_value(&integration_preview(&fixture.state, &cli).unwrap())
    }

    fn inspection_document(fixture: &InspectionFixture, command: Cmd) -> serde_json::Value {
        let cli = Cli {
            project_name: Some("proj".to_string()),
            project_scope: false,
            workspace_name: Some("child".to_string()),
            color: ColorChoice::Never,
            interactive: false,
            no_interactive: true,
            quiet: false,
            verbose: false,
            json: true,
            preview: false,
            command,
        };
        let mut out = test_channel();
        match &cli.command {
            Cmd::Inspect { target } => cmd_inspect(&fixture.state, &cli, &mut out, target).unwrap(),
            Cmd::Compare { workspace } => {
                cmd_compare(&fixture.state, &cli, &mut out, workspace).unwrap()
            }
            _ => unreachable!(),
        }
        assert_eq!(out.finish(), 0);
        one_document(out)
    }

    #[test]
    fn workspace_branch_updates_only_the_recorded_branch() {
        let fixture = inspection_fixture();
        let ws = Workspace::new(fixture.project.clone(), "child".to_string());
        for (value, unset, expected) in [
            (Some("replacement"), false, Some("replacement")),
            (None, false, Some("replacement")),
            (None, true, None),
            (None, false, None),
        ] {
            let cli = Cli {
                project_name: Some("proj".to_string()),
                project_scope: false,
                workspace_name: Some("child".to_string()),
                color: ColorChoice::Never,
                interactive: false,
                no_interactive: true,
                quiet: false,
                verbose: false,
                json: true,
                preview: false,
                command: Cmd::WorkspaceBranch {
                    set: value.map(str::to_string),
                    unset,
                },
            };
            let mut out = test_channel();
            workspace_branch(&ws, &cli, &mut out, value, unset).unwrap();
            assert_eq!(out.finish(), 0);
            let document = one_document(out);
            assert_eq!(document["workspace"], "child");
            assert_eq!(document["value"], json!(expected));
            let meta = fixture.project.metadata().read().unwrap();
            assert_eq!(meta.workspaces["child"].branch.as_deref(), expected);
            assert_eq!(
                meta.workspaces["child"].parent.as_ref().unwrap().name,
                "root"
            );
        }
        assert_eq!(
            fixture
                .project
                .metadata()
                .read()
                .unwrap()
                .default
                .as_deref(),
            Some("child")
        );
        assert_eq!(
            Workspace::new(fixture.project.clone(), "child".to_string())
                .worktree()
                .unwrap()
                .get_branch()
                .as_deref(),
            Some("child")
        );
    }

    #[test]
    fn comparison_clean_workspace_has_no_dirty_paths_or_rewrite() {
        let fixture = inspection_fixture();
        let value = inspection_document(
            &fixture,
            Cmd::Compare {
                workspace: "root".to_string(),
            },
        );
        assert_eq!(value["dirty_paths"], json!([]));
        assert_eq!(value["rewritten"], false);
        assert_eq!(value["paths"], json!([]));
    }

    #[test]
    fn comparison_dirty_workspace_lists_sorted_worktree_paths() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("dirty.txt"), "dirty\n").unwrap();
        let value = inspection_document(
            &fixture,
            Cmd::Compare {
                workspace: "root".to_string(),
            },
        );
        assert_eq!(value["dirty_paths"], json!(["dirty.txt"]));
    }

    #[test]
    fn comparison_divergent_workspaces_reports_commits_and_paths() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.child.join("child.txt"), "child\n").unwrap();
        git(&fixture.child, &["add", "."]);
        git(&fixture.child, &["commit", "-q", "-m", "child"]);
        std::fs::write(fixture.root.join("root.txt"), "root\n").unwrap();
        git(&fixture.root, &["add", "."]);
        git(&fixture.root, &["commit", "-q", "-m", "root"]);
        let value = inspection_document(
            &fixture,
            Cmd::Compare {
                workspace: "root".to_string(),
            },
        );
        assert_eq!(
            value["commits"]["current_only"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            value["commits"]["workspace_only"].as_array().unwrap().len(),
            1
        );
        assert_eq!(value["paths"][0]["path"], "root.txt");
    }

    #[test]
    fn squash_recovery_keeps_one_commit_after_a_conflict() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.child.join("conflict.txt"), "source\n").unwrap();
        git(&fixture.child, &["add", "conflict.txt"]);
        git(&fixture.child, &["commit", "-q", "-m", "source"]);
        std::fs::write(fixture.root.join("conflict.txt"), "dest\n").unwrap();
        git(&fixture.root, &["add", "conflict.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "dest"]);
        let starting_head = Sha::from_hex(&git_head(&fixture.root)).unwrap();
        let root = Workspace::new(fixture.project.clone(), "root".to_string());
        let wt = root.worktree().unwrap();
        let lock = fixture.project.lock("test").unwrap();
        let source = Sha::from_hex(&git_head(&fixture.child)).unwrap();
        let run = wt.stage_commits_captured(&lock, &[source]).unwrap();
        assert!(!run.status.success());
        std::fs::write(fixture.root.join("conflict.txt"), "resolved\n").unwrap();
        git(&fixture.root, &["add", "conflict.txt"]);
        let integration = IntegrationState {
            original_base: None,
            target: Some(ParentEdge {
                name: "root".to_string(),
                base: starting_head,
            }),
            mode: "merge".to_string(),
            squash_message: Some("squashed".to_string()),
        };
        finish_integration_recovery(&fixture.project, &root, &wt, &lock, &integration, true)
            .unwrap();
        assert_eq!(
            wt.list_revisions(&starting_head, &wt.head().unwrap())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("conflict.txt")).unwrap(),
            "resolved\n"
        );
    }

    #[test]
    fn interactive_merge_includes_unstaged_only_changes() {
        if std::env::var_os("SUBSPACE_UNSTAGED_MERGE_TEST").is_none() {
            let home = tempfile::tempdir().unwrap();
            let mut child = process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::interactive_merge_includes_unstaged_only_changes",
                ])
                .env("SUBSPACE_UNSTAGED_MERGE_TEST", "1")
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join("config"))
                .env("XDG_STATE_HOME", home.path().join("state"))
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped())
                .stderr(process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"y\n").unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let fixture = inspection_fixture();
        std::fs::write(fixture.child.join("base.txt"), "unstaged\n").unwrap();
        let project =
            Project::new("proj".to_string(), fixture.project.path().to_path_buf()).unwrap();
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                *meta = fixture.project.metadata().read()?;
                Ok(())
            })
            .unwrap();
        drop(lock);
        let cli = Cli {
            project_name: Some("proj".to_string()),
            project_scope: false,
            workspace_name: Some("root".to_string()),
            color: ColorChoice::Never,
            interactive: true,
            no_interactive: false,
            quiet: false,
            verbose: false,
            json: true,
            preview: false,
            command: Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: false,
                unstaged: true,
                remove: false,
                delete_branch: false,
            },
        };
        let mut out = output::Stdio::stdio(true, Verbosity::Normal);
        cmd_merge(
            &fixture.state,
            &cli,
            &HookRunner::new(Default::default(), subspace::hook::Stdout::Diagnostic),
            ClientSettings {
                color: false,
                interactive: true,
                verbosity: Verbosity::Normal,
            },
            &mut out,
            "child",
            None,
            false,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("base.txt")).unwrap(),
            "unstaged\n"
        );
    }

    #[test]
    fn interactive_merge_does_not_prompt_to_remove_without_flag() {
        if std::env::var_os("SUBSPACE_MERGE_REMOVAL_TEST").is_none() {
            let home = tempfile::tempdir().unwrap();
            let mut child = process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::interactive_merge_does_not_prompt_to_remove_without_flag",
                ])
                .env("SUBSPACE_MERGE_REMOVAL_TEST", "1")
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join("config"))
                .env("XDG_STATE_HOME", home.path().join("state"))
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped())
                .stderr(process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"y\ny\n").unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("remove proj/child?"));
            return;
        }
        let fixture = inspection_fixture();
        let lock = fixture.project.lock("test").unwrap();
        fixture
            .project
            .metadata()
            .update(&lock, |meta| {
                meta.default = Some("root".to_string());
                Ok(())
            })
            .unwrap();
        drop(lock);
        std::fs::write(fixture.child.join("new.txt"), "new\n").unwrap();
        git(&fixture.child, &["add", "new.txt"]);
        git(&fixture.child, &["commit", "-q", "-m", "new"]);
        let project =
            Project::new("proj".to_string(), fixture.project.path().to_path_buf()).unwrap();
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                *meta = fixture.project.metadata().read()?;
                Ok(())
            })
            .unwrap();
        drop(lock);
        let cli = Cli {
            project_name: Some("proj".to_string()),
            project_scope: false,
            workspace_name: Some("root".to_string()),
            color: ColorChoice::Never,
            interactive: true,
            no_interactive: false,
            quiet: false,
            verbose: false,
            json: true,
            preview: false,
            command: Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: false,
                unstaged: false,
                remove: false,
                delete_branch: false,
            },
        };
        let mut out = output::Stdio::stdio(true, Verbosity::Normal);
        cmd_merge(
            &fixture.state,
            &cli,
            &HookRunner::new(Default::default(), subspace::hook::Stdout::Diagnostic),
            ClientSettings {
                color: false,
                interactive: true,
                verbosity: Verbosity::Normal,
            },
            &mut out,
            "child",
            None,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(fixture.child.exists());
        assert!(
            project
                .metadata()
                .read()
                .unwrap()
                .workspaces
                .contains_key("child")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("new.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn empty_squash_does_not_leave_recovery_state() {
        if std::env::var_os("SUBSPACE_EMPTY_SQUASH_TEST").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::empty_squash_does_not_leave_recovery_state",
                ])
                .env("SUBSPACE_EMPTY_SQUASH_TEST", "1")
                .env("HOME", home.path())
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("temporary.txt"), "temporary\n").unwrap();
        git(&fixture.root, &["add", "temporary.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "add temporary"]);
        git(&fixture.root, &["rm", "-q", "temporary.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "remove temporary"]);
        let original_head = git_head(&fixture.child);
        let project =
            Project::new("proj".to_string(), fixture.project.path().to_path_buf()).unwrap();
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                *meta = fixture.project.metadata().read()?;
                Ok(())
            })
            .unwrap();
        drop(lock);
        let cli = Cli {
            project_name: Some("proj".to_string()),
            project_scope: false,
            workspace_name: Some("child".to_string()),
            color: ColorChoice::Never,
            interactive: false,
            no_interactive: true,
            quiet: false,
            verbose: false,
            json: true,
            preview: false,
            command: Cmd::Merge {
                source: "root".to_string(),
                squash: Some("squashed".to_string()),
                all: true,
                unstaged: false,
                remove: false,
                delete_branch: false,
            },
        };
        let mut out = output::Stdio::stdio(true, Verbosity::Normal);
        let result = cmd_merge(
            &fixture.state,
            &cli,
            &HookRunner::new(Default::default(), subspace::hook::Stdout::Diagnostic),
            ClientSettings {
                color: false,
                interactive: false,
                verbosity: Verbosity::Normal,
            },
            &mut out,
            "root",
            Some("squashed"),
            true,
            false,
            false,
            false,
        );
        assert_eq!(result.unwrap_err().to_string(), "nothing to merge");
        assert_eq!(git_head(&fixture.child), original_head);
        assert!(
            project.metadata().read().unwrap().workspaces["child"]
                .integration
                .is_none()
        );
        assert!(
            Workspace::new(fixture.project.clone(), "child".to_string())
                .worktree()
                .unwrap()
                .operation_in_progress()
                .is_none()
        );
    }

    #[test]
    fn merge_refuses_to_replace_unfinished_integration() {
        if std::env::var_os("SUBSPACE_INTEGRATION_TEST").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::merge_refuses_to_replace_unfinished_integration",
                ])
                .env("SUBSPACE_INTEGRATION_TEST", "1")
                .env("HOME", home.path())
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("new.txt"), "new\n").unwrap();
        git(&fixture.root, &["add", "new.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "new"]);
        let project =
            Project::new("proj".to_string(), fixture.project.path().to_path_buf()).unwrap();
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                *meta = fixture.project.metadata().read()?;
                meta.workspaces.get_mut("child").unwrap().integration = Some(IntegrationState {
                    original_base: None,
                    target: None,
                    mode: "merge".to_string(),
                    squash_message: Some("unfinished".to_string()),
                });
                Ok(())
            })
            .unwrap();
        drop(lock);
        let cli = Cli {
            project_name: Some("proj".to_string()),
            project_scope: false,
            workspace_name: Some("child".to_string()),
            color: ColorChoice::Never,
            interactive: false,
            no_interactive: true,
            quiet: false,
            verbose: false,
            json: true,
            preview: false,
            command: Cmd::Merge {
                source: "root".to_string(),
                squash: None,
                all: true,
                unstaged: false,
                remove: false,
                delete_branch: false,
            },
        };
        let mut out = output::Stdio::stdio(true, Verbosity::Normal);
        assert!(
            cmd_merge(
                &fixture.state,
                &cli,
                &HookRunner::new(Default::default(), subspace::hook::Stdout::Diagnostic),
                ClientSettings {
                    color: false,
                    interactive: false,
                    verbosity: Verbosity::Normal
                },
                &mut out,
                "root",
                None,
                true,
                false,
                false,
                false
            )
            .is_err()
        );
        assert_eq!(
            project.metadata().read().unwrap().workspaces["child"]
                .integration
                .as_ref()
                .unwrap()
                .squash_message
                .as_deref(),
            Some("unfinished")
        );
        assert!(!fixture.child.join("new.txt").exists());
    }

    #[test]
    fn merge_selects_the_source_stack_or_the_common_ancestor() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("parent.txt"), "parent\n").unwrap();
        git(&fixture.root, &["add", "parent.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "parent"]);
        let parent_tip = git_head(&fixture.root);
        let stacked = fixture.project.worktrees_path().join("stacked");
        git(
            &fixture.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "stacked",
                stacked.to_str().unwrap(),
            ],
        );
        std::fs::write(stacked.join("source.txt"), "source\n").unwrap();
        git(&stacked, &["add", "source.txt"]);
        git(&stacked, &["commit", "-q", "-m", "source"]);
        let source_tip = git_head(&stacked);
        let lock = fixture.project.lock("test").unwrap();
        fixture
            .project
            .metadata()
            .update(&lock, |meta| {
                meta.workspaces.insert(
                    "stacked".to_string(),
                    WorkspaceMeta {
                        parent: Some(ParentEdge {
                            name: "root".to_string(),
                            base: Sha::from_hex(&parent_tip).unwrap(),
                        }),
                        branch: Some("stacked".to_string()),
                        integration: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        drop(lock);
        let child = Workspace::new(fixture.project.clone(), "child".to_string());
        let wt = child.worktree().unwrap();
        let source = Sha::from_hex(&source_tip).unwrap();
        let edge = fixture.project.metadata().read().unwrap().workspaces["stacked"]
            .parent
            .clone()
            .unwrap();
        let missing = merge_commits(&wt, &wt.head().unwrap(), &source, None, false).unwrap_err();
        assert!(missing.to_string().contains("use --all"));
        let (full, _, common) =
            merge_commits(&wt, &wt.head().unwrap(), &source, Some(&edge), true).unwrap();
        assert_eq!(full.len(), 2);
        assert_eq!(full[1], source);
        assert_ne!(common, edge.base);
        let (selected, _, anchor) =
            merge_commits(&wt, &wt.head().unwrap(), &source, Some(&edge), false).unwrap();
        assert_eq!(selected, vec![source]);
        assert_eq!(anchor, edge.base);
        let lock = fixture.project.lock("test").unwrap();
        let result = wt.stage_commits_captured(&lock, &selected).unwrap();
        assert!(result.status.success());
        let result = wt.create_commit_captured(&lock, "squash").unwrap();
        assert!(result.status.success());
        assert!(fixture.child.join("source.txt").exists());
        assert!(!fixture.child.join("parent.txt").exists());
    }

    #[test]
    fn integration_preview_reports_a_clean_merge_and_requested_unstaged_paths() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.child.join("dirty.txt"), "dirty\n").unwrap();
        let value = integration_document(
            &fixture,
            "root",
            Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: true,
                unstaged: true,
                remove: false,
                delete_branch: false,
            },
        );
        assert!(value["plan"]["effects"].is_object());
        assert!(value["plan"]["warnings"].is_array());
        assert!(value["plan"]["confirmation"].is_string());
        assert!(value["plan"].get("token").is_none());
        assert_eq!(value["plan"]["dropped_commits"], json!([]));
        assert_eq!(value["plan"]["overlap_paths"], json!([]));
        assert_eq!(
            value["plan"]["requested_unstaged_paths"],
            json!(["dirty.txt"])
        );
    }

    #[test]
    fn integration_preview_reports_a_clean_rebase_and_not_a_stopped_rebase() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.child.join("child.txt"), "child\n").unwrap();
        git(&fixture.child, &["add", "."]);
        git(&fixture.child, &["commit", "-q", "-m", "child"]);
        let value = integration_document(
            &fixture,
            "child",
            Cmd::Rebase {
                base: Some("root".to_string()),
                interactive: false,
                all: false,
            },
        );
        assert_eq!(value["plan"]["stopped"], false);
        assert_eq!(value["plan"]["anchors"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["plan"]["applied_commits"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn integration_preview_reports_staged_and_unstaged_merge_paths() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.child.join("staged.txt"), "staged\n").unwrap();
        git(&fixture.child, &["add", "staged.txt"]);
        std::fs::write(fixture.child.join("dirty.txt"), "dirty\n").unwrap();
        let value = integration_document(
            &fixture,
            "root",
            Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: true,
                unstaged: true,
                remove: false,
                delete_branch: false,
            },
        );
        assert_eq!(
            value["plan"]["requested_unstaged_paths"],
            json!(["dirty.txt", "staged.txt"])
        );
    }

    #[test]
    fn integration_preview_reports_dropped_patches_and_rewritten_anchors() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("same.txt"), "same\n").unwrap();
        git(&fixture.root, &["add", "same.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "parent patch"]);
        std::fs::write(fixture.child.join("same.txt"), "same\n").unwrap();
        git(&fixture.child, &["add", "same.txt"]);
        git(&fixture.child, &["commit", "-q", "-m", "child patch"]);
        let value = integration_document(
            &fixture,
            "root",
            Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: true,
                unstaged: false,
                remove: false,
                delete_branch: false,
            },
        );
        assert_eq!(
            value["plan"]["dropped_commits"].as_array().unwrap().len(),
            1
        );
        assert_eq!(value["plan"]["overlap_paths"], json!(["same.txt"]));
        assert_eq!(value["plan"]["requested_unstaged_paths"], json!([]));
    }

    #[test]
    fn integration_preview_reports_source_untracked_over_destination_tracked() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("collision.txt"), "tracked\n").unwrap();
        git(&fixture.root, &["add", "collision.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "tracked collision"]);
        std::fs::write(fixture.child.join("collision.txt"), "untracked\n").unwrap();
        let value = integration_document(
            &fixture,
            "root",
            Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: true,
                unstaged: false,
                remove: false,
                delete_branch: false,
            },
        );
        assert_eq!(
            value["plan"]["untracked_collisions"],
            json!(["collision.txt"])
        );
        assert_eq!(value["plan"]["requested_unstaged_paths"], json!([]));
    }

    #[test]
    fn integration_preview_reports_rebase_drops_and_committed_overlap() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("same.txt"), "same\n").unwrap();
        git(&fixture.root, &["add", "same.txt"]);
        git(&fixture.root, &["commit", "-q", "-m", "parent patch"]);
        std::fs::write(fixture.child.join("same.txt"), "same\n").unwrap();
        git(&fixture.child, &["add", "same.txt"]);
        git(&fixture.child, &["commit", "-q", "-m", "child patch"]);
        let value = integration_document(
            &fixture,
            "child",
            Cmd::Rebase {
                base: Some("root".to_string()),
                interactive: false,
                all: false,
            },
        );
        assert_eq!(
            value["plan"]["dropped_commits"].as_array().unwrap().len(),
            1
        );
        assert_eq!(value["plan"]["changed_paths"], json!(["same.txt"]));
        assert_eq!(value["plan"]["overlap_paths"], json!(["same.txt"]));
    }

    #[test]
    fn rebase_recovery_keeps_state_after_a_second_conflict() {
        let fixture = inspection_fixture();
        start_conflicted_rebase(&fixture, true);
        git(&fixture.child, &["add", "first.txt"]);

        let lock = fixture.project.lock("test").unwrap();
        let workspace = Workspace::new(fixture.project.clone(), "child".to_string());
        let worktree = Worktree::get(&fixture.child).unwrap();
        let integration = fixture
            .project
            .metadata()
            .read()
            .unwrap()
            .workspaces
            .get("child")
            .unwrap()
            .integration
            .clone()
            .unwrap();
        let error = finish_integration_recovery(
            &fixture.project,
            &workspace,
            &worktree,
            &lock,
            &integration,
            true,
        )
        .unwrap_err();
        drop(lock);
        assert!(error.to_string().contains("git rebase failed"), "{error:#}");
        assert!(matches!(
            fixture
                .project
                .metadata()
                .read()
                .unwrap()
                .workspaces
                .get("child")
                .unwrap()
                .integration,
            Some(_)
        ));
        assert!(matches!(
            Worktree::get(&fixture.child)
                .unwrap()
                .operation_in_progress(),
            Some(InProgressOperation::Rebase { .. })
        ));
    }

    #[test]
    fn rebase_recovery_keeps_state_when_abort_fails() {
        let fixture = inspection_fixture();
        start_conflicted_rebase(&fixture, false);
        git(&fixture.child, &["rebase", "--abort"]);

        let lock = fixture.project.lock("test").unwrap();
        let workspace = Workspace::new(fixture.project.clone(), "child".to_string());
        let worktree = Worktree::get(&fixture.child).unwrap();
        let integration = fixture
            .project
            .metadata()
            .read()
            .unwrap()
            .workspaces
            .get("child")
            .unwrap()
            .integration
            .clone()
            .unwrap();
        let error = finish_integration_recovery(
            &fixture.project,
            &workspace,
            &worktree,
            &lock,
            &integration,
            false,
        )
        .unwrap_err();
        drop(lock);
        assert!(error.to_string().contains("git rebase failed"), "{error:#}");
        assert!(
            fixture
                .project
                .metadata()
                .read()
                .unwrap()
                .workspaces
                .get("child")
                .unwrap()
                .integration
                .is_some()
        );
    }

    #[test]
    fn merge_recovery_continues_a_stored_merge() {
        let fixture = inspection_fixture();
        start_conflicted_merge(&fixture);
        std::fs::write(fixture.root.join("merge.txt"), "resolved\n").unwrap();
        git(&fixture.root, &["add", "merge.txt"]);

        let lock = fixture.project.lock("test").unwrap();
        let workspace = Workspace::new(fixture.project.clone(), "root".to_string());
        let worktree = Worktree::get(&fixture.root).unwrap();
        let integration = fixture
            .project
            .metadata()
            .read()
            .unwrap()
            .workspaces
            .get("root")
            .unwrap()
            .integration
            .clone()
            .unwrap();
        let recovery = finish_integration_recovery(
            &fixture.project,
            &workspace,
            &worktree,
            &lock,
            &integration,
            true,
        )
        .unwrap();
        assert_eq!(recovery.operation, "merge");
        assert_eq!(recovery.outcome, "continued");
        assert!(
            fixture
                .project
                .metadata()
                .read()
                .unwrap()
                .workspaces
                .get("root")
                .unwrap()
                .integration
                .is_none()
        );
    }

    #[test]
    fn merge_recovery_keeps_state_when_abort_fails() {
        let fixture = inspection_fixture();
        start_conflicted_merge(&fixture);
        git(&fixture.root, &["cherry-pick", "--abort"]);

        let lock = fixture.project.lock("test").unwrap();
        let workspace = Workspace::new(fixture.project.clone(), "root".to_string());
        let worktree = Worktree::get(&fixture.root).unwrap();
        let integration = fixture
            .project
            .metadata()
            .read()
            .unwrap()
            .workspaces
            .get("root")
            .unwrap()
            .integration
            .clone()
            .unwrap();
        let error = finish_integration_recovery(
            &fixture.project,
            &workspace,
            &worktree,
            &lock,
            &integration,
            false,
        )
        .unwrap_err();
        drop(lock);
        assert!(error.to_string().contains("git merge failed"), "{error:#}");
        assert!(
            fixture
                .project
                .metadata()
                .read()
                .unwrap()
                .workspaces
                .get("root")
                .unwrap()
                .integration
                .is_some()
        );
    }

    #[test]
    fn integration_preview_reports_untracked_collisions() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("collision.txt"), "root\n").unwrap();
        std::fs::write(fixture.child.join("collision.txt"), "child\n").unwrap();
        let value = integration_document(
            &fixture,
            "root",
            Cmd::Merge {
                source: "child".to_string(),
                squash: None,
                all: true,
                unstaged: true,
                remove: false,
                delete_branch: false,
            },
        );
        assert_eq!(
            value["plan"]["untracked_collisions"],
            json!(["collision.txt"])
        );
    }

    #[test]
    fn comparison_rewritten_parent_reports_rewrite_state() {
        let fixture = inspection_fixture();
        std::fs::write(fixture.root.join("base.txt"), "rewritten\n").unwrap();
        git(&fixture.root, &["add", "base.txt"]);
        git(
            &fixture.root,
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let meta = fixture.project.metadata().read().unwrap();
        assert!(
            Workspace::get(&fixture.project, "child")
                .unwrap()
                .info(&meta)
                .unwrap()
                .parent
                .unwrap()
                .rewritten
        );
        let value = inspection_document(
            &fixture,
            Cmd::Compare {
                workspace: "root".to_string(),
            },
        );
        assert_eq!(value["current"], "child");
        assert_eq!(value["workspace"], "root");
        assert!(value.get("rewritten").is_some());
    }

    #[test]
    fn workspace_inspection_reports_identity_and_omits_content_and_patches() {
        let fixture = inspection_fixture();
        let value = inspection_document(
            &fixture,
            Cmd::Inspect {
                target: InspectTarget::Workspace,
            },
        );
        assert_eq!(value["project"], "proj");
        assert_eq!(value["name"], "child");
        assert!(value.get("parent").is_some());
        assert!(value.get("note_path").is_none());
        assert!(value.get("patch").is_none());
        assert!(value.get("diff").is_none());
    }

    #[test]
    fn project_inspection_reports_settings_and_repair_state() {
        let fixture = inspection_fixture();
        let value = inspection_document(
            &fixture,
            Cmd::Inspect {
                target: InspectTarget::Project,
            },
        );
        let meta = fixture.project.metadata().read().unwrap();
        assert_eq!(meta.default.as_deref(), Some("child"));
        assert_eq!(meta.branch_prefix.as_deref(), Some("topic"));
        assert!(value.get("repair").is_some());
        assert_eq!(value["repair"]["pruned"], json!([]));
        assert_eq!(value["repair"]["reparented"], json!([]));
    }

    #[test]
    fn machine_capabilities_report_the_action_contract() {
        let value = machine_capabilities();
        assert_eq!(value["protocol"], "subspace-json");
        assert_eq!(value["version"], 2);
        let features = value["features"].as_array().unwrap();
        assert!(features.contains(&json!("action-contract")));
        assert!(features.contains(&json!("direct-mutation")));
        assert!(features.contains(&json!("mutation-preview")));
        assert!(value["groups"]["commands"].as_array().unwrap().is_empty());
        assert!(value["actions"]["inspectWorkspace"].is_object());
        for action in ["mergePreview", "merge"] {
            assert_eq!(
                value["actions"][action]["fields"],
                json!([
                    "mergeSource",
                    "squash",
                    "all",
                    "unstaged",
                    "remove",
                    "deleteBranch"
                ])
            );
        }
        assert!(value["groups"].get("comments").is_none());
        for action in [
            "notePath",
            "noteShow",
            "noteCreate",
            "commentList",
            "feedbackAdd",
            "instructionAdd",
        ] {
            assert!(value["actions"].get(action).is_none());
        }
        assert!(value["actions"].get("git").is_none());
        for action in value["actions"].as_object().unwrap().values() {
            for field in [
                "description",
                "group",
                "effect",
                "fields",
                "required",
                "constraints",
                "arguments",
                "result",
            ] {
                assert!(action.get(field).is_some(), "action has no {field}");
            }
        }
    }

    #[test]
    fn machine_read_actions_cover_inspection_and_comparison() {
        for argv in [
            ["subspace", "inspect", "workspace"].as_slice(),
            ["subspace", "inspect", "project"].as_slice(),
            ["subspace", "compare", "root"].as_slice(),
        ] {
            assert!(
                json_supported(&Cli::parse_from(argv).command),
                "missing read action: {argv:?}"
            );
        }
    }

    /// Every operation the Pi tool can call, as the argv that selects it.
    const TOOL_OPERATIONS: &[&[&str]] = &[
        &["subspace", "capabilities"],
        &["subspace", "current"],
        &["subspace", "path"],
        &["subspace", "root"],
        &["subspace", "parent"],
        &["subspace", "children"],
        &["subspace", "siblings"],
        &["subspace", "default"],
        &["subspace", "branch-prefix"],
        &["subspace", "workspace-branch"],
        &["subspace", "list"],
        &["subspace", "show"],
        &["subspace", "inspect", "workspace"],
        &["subspace", "inspect", "project"],
        &["subspace", "compare", "other"],
        &["subspace", "tree"],
        &["subspace", "diff"],
        &["subspace", "log"],
        &["subspace", "project", "current"],
        &["subspace", "project", "list"],
        &["subspace", "project", "remove", "tide"],
        &["subspace", "project", "rename", "tide", "flow"],
        &["subspace", "project", "move", "tide", "/p"],
        &["subspace", "project", "clone", "git@host:tide.git"],
        &["subspace", "project", "init", "/p/tide"],
        &["subspace", "add", "work"],
        &["subspace", "branch", "work"],
        &["subspace", "remove", "work"],
        &["subspace", "rename", "work", "next"],
        &["subspace", "repair"],
        &["subspace", "merge", "work"],
        &["subspace", "rebase"],
        &["subspace", "integration", "continue"],
        &["subspace", "integration", "abort"],
        &["subspace", "snapshot"],
        &["subspace", "snapshot", "create"],
        &["subspace", "snapshot", "list"],
        &["subspace", "snapshot", "restore", "work.tar"],
    ];

    /// Every operation the tool omits, with the rejection code that JSON mode gives it.
    const OMITTED_OPERATIONS: &[(&[&str], &str)] =
        &[(&["subspace", "rebase", "-i"], "unsupported")];

    /// The name of the operation that `cmd` selects: its subcommand path, and the flag that splits
    /// an interactive form from a plain one. The JSON matrix keys its rows on this name, so one row
    /// covers one operation and no operation gets two rows.
    fn operation_form(cmd: &Cmd) -> String {
        match cmd {
            Cmd::Capabilities => "capabilities".to_string(),
            Cmd::Current => "current".to_string(),
            Cmd::Path => "path".to_string(),
            Cmd::Root => "root".to_string(),
            Cmd::Parent => "parent".to_string(),
            Cmd::Children => "children".to_string(),
            Cmd::Siblings => "siblings".to_string(),
            Cmd::Default { .. } => "default".to_string(),
            Cmd::BranchPrefix { .. } => "branch-prefix".to_string(),
            Cmd::WorkspaceBranch { .. } => "workspace-branch".to_string(),
            Cmd::List { .. } => "list".to_string(),
            Cmd::Show => "show".to_string(),
            Cmd::Inspect { target } => format!(
                "inspect {}",
                match target {
                    InspectTarget::Workspace => "workspace",
                    InspectTarget::Project => "project",
                }
            ),
            Cmd::Compare { .. } => "compare".to_string(),
            Cmd::Tree => "tree".to_string(),
            Cmd::Diff { .. } => "diff".to_string(),
            Cmd::Log { .. } => "log".to_string(),
            Cmd::Add { .. } => "add".to_string(),
            Cmd::Branch { .. } => "branch".to_string(),
            Cmd::Remove { .. } => "remove".to_string(),
            Cmd::Rename { .. } => "rename".to_string(),
            Cmd::Repair { .. } => "repair".to_string(),
            Cmd::Merge { .. } => "merge".to_string(),
            Cmd::Rebase { interactive, .. } => {
                format!("rebase{}", if *interactive { " -i" } else { "" })
            }
            Cmd::Integration { action } => format!(
                "integration {}",
                match action {
                    IntegrationAction::Continue => "continue",
                    IntegrationAction::Abort => "abort",
                }
            ),
            Cmd::Project { action } => format!(
                "project {}",
                match action {
                    ProjectAction::Current => "current",
                    ProjectAction::List => "list",
                    ProjectAction::Remove { .. } => "remove",
                    ProjectAction::Rename { .. } => "rename",
                    ProjectAction::Move { .. } => "move",
                    ProjectAction::Clone { .. } => "clone",
                    ProjectAction::Init { .. } => "init",
                }
            ),
            Cmd::Snapshot { action } => match action {
                None => "snapshot".to_string(),
                Some(SnapshotAction::Create { .. }) => "snapshot create".to_string(),
                Some(SnapshotAction::List) => "snapshot list".to_string(),
                Some(SnapshotAction::Restore { .. }) => "snapshot restore".to_string(),
            },
        }
    }

    /// The matrix names that one clap leaf demands: its own path, plus the interactive form when
    /// the leaf carries an `interactive` argument.
    fn expected_forms(path: &str, leaf: &clap::Command) -> Vec<String> {
        let mut forms = vec![path.to_string()];
        if leaf
            .get_arguments()
            .any(|a| a.get_id().as_str().ends_with("interactive"))
        {
            forms.push(format!("{path} -i"));
        }
        forms
    }

    /// Every command form that the clap tree accepts, as the matrix names it. A parent that parses
    /// on its own, such as `snapshot`, contributes its own name next to its children.
    fn clap_leaf_forms(prefix: &str, command: &clap::Command) -> Vec<String> {
        let mut forms = Vec::new();
        for sub in command.get_subcommands() {
            let path = if prefix.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{prefix} {}", sub.get_name())
            };
            if sub.get_subcommands().next().is_none() {
                forms.extend(expected_forms(&path, sub));
                continue;
            }
            if !sub.is_subcommand_required_set() {
                forms.extend(expected_forms(&path, sub));
            }
            forms.extend(clap_leaf_forms(&path, sub));
        }
        forms
    }

    #[test]
    /// The JSON matrix: each tool operation has a machine result, each omitted operation has its
    /// rejection code, and no operation sits in both lists or twice in one.
    fn json_matrix_covers_every_operation_once() {
        let mut forms: BTreeMap<String, &str> = BTreeMap::new();
        for argv in TOOL_OPERATIONS {
            let cli = Cli::parse_from(*argv);
            assert!(
                json_supported(&cli.command),
                "{argv:?} has no machine result"
            );
            let form = operation_form(&cli.command);
            assert!(
                forms.insert(form.clone(), "tool").is_none(),
                "{form} appears more than once in the matrix"
            );
        }
        for (argv, code) in OMITTED_OPERATIONS {
            let cli = Cli::parse_from(*argv);
            assert!(
                !json_supported(&cli.command),
                "{argv:?} must have no machine result"
            );
            assert_eq!(
                json_rejection(&cli.command).0,
                *code,
                "{argv:?} gives the wrong rejection code"
            );
            let form = operation_form(&cli.command);
            assert!(
                forms.insert(form.clone(), "omitted").is_none(),
                "{form} appears more than once in the matrix"
            );
        }
        for expected in clap_leaf_forms("", &Cli::command()) {
            assert!(
                forms.contains_key(&expected),
                "the matrix misses the {expected} command form"
            );
        }
    }

    #[test]
    fn cli_command_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn rebase_parses_an_optional_base_and_its_flags() {
        let bare = Cli::parse_from(["subspace", "rebase"]);
        match &bare.command {
            Cmd::Rebase {
                base,
                interactive,
                all,
                ..
            } => {
                assert_eq!(*base, None);
                assert!(!interactive);
                assert!(!all);
            }
            _ => panic!("expected a rebase command"),
        }

        let named = Cli::parse_from(["subspace", "rebase", "-i", "@parent", "--all"]);
        match &named.command {
            Cmd::Rebase {
                base,
                interactive,
                all,
                ..
            } => {
                assert_eq!(base.as_deref(), Some("@parent"));
                assert!(interactive);
                assert!(all);
            }
            _ => panic!("expected a rebase command"),
        }
    }

    #[test]
    fn removed_commands_are_rejected() {
        for command in ["note", "comment", "feedback", "instruction"] {
            assert!(Cli::try_parse_from(["subspace", command]).is_err());
        }
        for action in ["add", "import"] {
            assert!(Cli::try_parse_from(["subspace", "project", action]).is_err());
        }
        assert!(Cli::try_parse_from(["subspace", "config", "path"]).is_err());
        assert!(Cli::try_parse_from(["subspace", "config", "show", "settings"]).is_err());
        assert!(Cli::try_parse_from(["subspace", "config", "apply"]).is_err());
        assert!(Cli::try_parse_from(["subspace", "config", "show", "env"]).is_err());
        assert!(Cli::try_parse_from(["subspace", "exec", "work", "--", "true"]).is_err());
    }

    #[test]
    fn git_command_is_removed() {
        assert!(Cli::try_parse_from(["subspace", "git", "status"]).is_err());
    }

    /// Parses `argv` and returns the raw trailing arguments that its `diff` or `log` collected,
    /// with the parsed CLI that holds them.
    fn trailing(argv: &[&str]) -> (Cli, Vec<String>) {
        let cli = Cli::parse_from(argv);
        let args = match &cli.command {
            Cmd::Diff { args, .. } | Cmd::Log { args, .. } => args.clone(),
            _ => panic!("expected a diff or log command"),
        };
        (cli, args)
    }

    /// The git color argument and the other git arguments that `argv` resolves to, as `cmd_diff`
    /// and `cmd_log` build them.
    fn passthrough(argv: &[&str]) -> (String, Vec<String>) {
        passthrough_mode(argv, false)
    }

    /// Like [`passthrough`], but resolves the color as `cmd_diff` and `cmd_log` do in JSON mode.
    fn passthrough_mode(argv: &[&str], json: bool) -> (String, Vec<String>) {
        let (cli, args) = trailing(argv);
        let (settings, rest) = take_passthrough_color(cli.settings(false, false), json, &args);
        (git_color_arg(settings), rest)
    }

    #[test]
    fn clap_leaves_a_color_written_after_a_git_flag_in_the_trailing_arguments() {
        let (cli, args) = trailing(&["subspace", "diff", "--parent", "--stat", "--color", "never"]);
        assert!(matches!(cli.color, ColorChoice::Auto));
        assert_eq!(args, ["--stat", "--color", "never"]);
    }

    #[test]
    fn a_trailing_color_is_taken_in_both_forms_and_never_reaches_git() {
        let cases: [&[&str]; 4] = [
            &["subspace", "diff", "--parent", "--stat", "--color", "never"],
            &["subspace", "diff", "--parent", "--stat", "--color=never"],
            &["subspace", "log", "--parent", "--stat", "--color", "never"],
            &["subspace", "log", "--parent", "--stat", "--color=never"],
        ];
        for argv in cases {
            let (color, rest) = passthrough(argv);
            assert_eq!(color, "--color=never", "{argv:?}");
            assert_eq!(rest, ["--stat"], "{argv:?}");
        }
    }

    #[test]
    fn the_last_trailing_color_wins_over_an_earlier_one_and_the_global_flag() {
        let (color, rest) = passthrough(&[
            "subspace",
            "--color",
            "never",
            "diff",
            "--parent",
            "--stat",
            "--color",
            "never",
            "--color=always",
        ]);
        assert_eq!(color, "--color=always");
        assert_eq!(rest, ["--stat"]);
    }

    #[test]
    fn a_color_value_only_git_knows_is_left_for_git() {
        let (color, rest) = passthrough(&[
            "subspace",
            "--color",
            "always",
            "diff",
            "--parent",
            "--stat",
            "--color=true",
        ]);
        assert_eq!(color, "--color=always");
        assert_eq!(rest, ["--stat", "--color=true"]);
    }

    #[test]
    fn a_bare_color_and_the_token_after_it_are_left_for_git() {
        let (color, rest) = passthrough(&[
            "subspace", "--color", "always", "diff", "--stat", "--color", "HEAD~1",
        ]);
        assert_eq!(color, "--color=always");
        assert_eq!(rest, ["--stat", "--color", "HEAD~1"]);

        let (_, rest) = passthrough(&["subspace", "diff", "--parent", "--stat", "--color"]);
        assert_eq!(rest, ["--stat", "--color"]);
    }

    #[test]
    fn derive_clone_name_takes_the_last_segment_without_the_git_suffix() {
        assert_eq!(derive_clone_name("https://host/owner/repo.git"), "repo");
        assert_eq!(derive_clone_name("git@host:owner/repo.git"), "repo");
        assert_eq!(derive_clone_name("https://host/owner/repo/"), "repo");
        assert_eq!(derive_clone_name("repo"), "repo");
    }

    /// Runs a git command in `cwd` and asserts that it succeeds.
    fn git(cwd: &Path, args: &[&str]) {
        let status = process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["-c", "user.email=test@test", "-c", "user.name=test"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn test_channel() -> output::Channel<Vec<u8>, Vec<u8>, Vec<u8>> {
        output::Channel::new(Vec::new(), Vec::new(), Vec::new(), true, Verbosity::Normal)
    }

    fn one_document(channel: output::Channel<Vec<u8>, Vec<u8>, Vec<u8>>) -> serde_json::Value {
        let machine = String::from_utf8(channel.into_machine()).unwrap();
        let lines: Vec<&str> = machine.trim().lines().collect();
        assert_eq!(lines.len(), 1);
        serde_json::from_str(lines[0]).unwrap()
    }

    struct RemovalFixture {
        _dir: tempfile::TempDir,
        project: Project,
        root: PathBuf,
        parent: Workspace,
        child: Workspace,
    }

    fn removal_fixture() -> RemovalFixture {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::with_dirs(
            "remove-test".to_string(),
            dir.path().join("project"),
            dir.path().join("config"),
            dir.path().join("state"),
        );
        let worktrees = project.worktrees_path();
        std::fs::create_dir_all(&worktrees).unwrap();
        let root = worktrees.join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["commit", "-q", "--allow-empty", "-m", "initial"]);
        let root_head = git_head(&root);

        let parent_path = worktrees.join("parent");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "parent",
                parent_path.to_str().unwrap(),
                "HEAD",
            ],
        );
        let parent_head = git_head(&parent_path);
        let child_path = worktrees.join("child");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "child",
                child_path.to_str().unwrap(),
                "HEAD",
            ],
        );

        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |meta| {
                meta.workspaces
                    .insert("root".to_string(), WorkspaceMeta::default());
                meta.workspaces.insert(
                    "parent".to_string(),
                    WorkspaceMeta {
                        parent: Some(ParentEdge {
                            name: "root".to_string(),
                            base: Sha::from_hex(&root_head).unwrap(),
                        }),
                        branch: Some("parent".to_string()),
                        integration: None,
                    },
                );
                meta.workspaces.insert(
                    "child".to_string(),
                    WorkspaceMeta {
                        parent: Some(ParentEdge {
                            name: "parent".to_string(),
                            base: Sha::from_hex(&parent_head).unwrap(),
                        }),
                        branch: Some("child".to_string()),
                        integration: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        drop(lock);

        RemovalFixture {
            _dir: dir,
            parent: Workspace::new(project.clone(), "parent".to_string()),
            child: Workspace::new(project.clone(), "child".to_string()),
            project,
            root,
        }
    }

    #[test]
    fn repair_relink_fields_name_only_rebuilt_workspaces() {
        use subspace::hook::Values;
        use subspace::workspace::Relink;

        let fixture = removal_fixture();
        let relinks = vec![Relink {
            workspace: "child".to_string(),
            branch: "child".to_string(),
        }];
        let fields = repair_relink_fields(&fixture.project, &relinks);

        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].get("workspace"), Some("child"));
        assert_eq!(
            fields[0].get("path"),
            Some(fixture.child.path().to_str().unwrap())
        );
        assert!(repair_relink_fields(&fixture.project, &[]).is_empty());
    }

    #[test]
    fn remove_keeps_a_dirty_workspace() {
        let fixture = removal_fixture();
        std::fs::write(fixture.child.path().join("untracked.txt"), "keep").unwrap();
        let lock = fixture.project.lock("test").unwrap();
        let removals = remove_confirmed_workspaces(
            &lock,
            &fixture.project,
            std::slice::from_ref(&fixture.child),
            true,
        )
        .unwrap();
        assert_eq!(removals.failures.len(), 1);
        assert!(fixture.child.path().join("untracked.txt").exists());
        assert!(
            fixture
                .project
                .metadata()
                .read()
                .unwrap()
                .workspaces
                .contains_key("child")
        );
    }

    #[test]
    fn remove_does_not_delete_a_branch_named_after_a_detached_workspace() {
        let fixture = removal_fixture();
        let root_branch = fixture.child.worktree().unwrap().get_branch().unwrap();
        git(&fixture.child.path(), &["checkout", "--detach"]);
        git(&fixture.root, &["branch", "-m", &root_branch, "other"]);
        git(&fixture.root, &["branch", "child"]);
        let lock = fixture.project.lock("test").unwrap();
        let removals = remove_confirmed_workspaces(
            &lock,
            &fixture.project,
            std::slice::from_ref(&fixture.child),
            true,
        )
        .unwrap();
        assert!(removals.failures.is_empty());
        assert!(
            fixture
                .project
                .anchor_worktree()
                .unwrap()
                .has_branch("child")
        );
    }

    #[test]
    fn remove_keeps_a_locked_workspace_metadata_and_state() {
        let fixture = removal_fixture();
        std::fs::create_dir_all(fixture.child.state_path()).unwrap();
        std::fs::write(
            fixture.child.state_path().join("data.json"),
            "workspace data",
        )
        .unwrap();
        git(
            &fixture.root,
            &["worktree", "lock", fixture.child.path().to_str().unwrap()],
        );
        let lock = fixture.project.lock("test").unwrap();
        let removals = remove_confirmed_workspaces(
            &lock,
            &fixture.project,
            std::slice::from_ref(&fixture.child),
            false,
        )
        .unwrap();
        drop(lock);

        assert_eq!(removals.failures.len(), 1);
        let metadata = fixture.project.metadata().read().unwrap();
        assert!(metadata.workspaces.contains_key("child"));
        assert!(fixture.child.path().exists());
        assert!(fixture.child.state_path().join("data.json").exists());
    }

    #[test]
    fn remove_keeps_failed_targets_and_reports_only_confirmed_removals() {
        let fixture = removal_fixture();
        std::fs::create_dir_all(fixture.parent.state_path()).unwrap();
        std::fs::create_dir_all(fixture.child.state_path()).unwrap();
        std::fs::write(fixture.parent.state_path().join("data.json"), "removed").unwrap();
        std::fs::write(fixture.child.state_path().join("data.json"), "survives").unwrap();
        git(
            &fixture.root,
            &["worktree", "lock", fixture.child.path().to_str().unwrap()],
        );
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = log.clone();
        let hooks: subspace::hook::Hooks = serde_json::from_str(
            r#"{"workspace_remove_pre":"pre-remove {workspace}","workspace_remove_post":"post-remove {workspace}","workspace_reparent_post":"post-reparent {workspace}"}"#,
        )
        .unwrap();
        let hooks = HookRunner::with_exec(
            hooks,
            Box::new(move |command, _env| {
                recorded.borrow_mut().push(command.to_string());
                Ok(())
            }),
        );
        let lock = fixture.project.lock("test").unwrap();
        let removals = remove_confirmed_workspaces(
            &lock,
            &fixture.project,
            &[fixture.child.clone(), fixture.parent.clone()],
            false,
        )
        .unwrap();
        drop(lock);
        run_post_hooks(&hooks, &removals.pending).unwrap();

        assert_eq!(removals.failures.len(), 1);
        let metadata = fixture.project.metadata().read().unwrap();
        assert!(!metadata.workspaces.contains_key("parent"));
        assert_eq!(
            metadata.workspaces["child"].parent.as_ref().unwrap().name,
            "root"
        );
        assert!(!fixture.parent.path().exists());
        assert!(!fixture.parent.state_path().exists());
        assert!(fixture.child.path().exists());
        assert!(fixture.child.state_path().join("data.json").exists());
        assert!(
            log.borrow()
                .iter()
                .any(|entry| entry == "post-remove parent")
        );
        assert!(
            log.borrow()
                .iter()
                .any(|entry| entry == "post-reparent child")
        );
        assert!(
            !log.borrow()
                .iter()
                .any(|entry| entry == "post-remove child")
        );
    }

    #[test]
    fn merge_removal_keeps_failed_source_metadata_and_state() {
        let fixture = removal_fixture();
        std::fs::create_dir_all(fixture.parent.state_path()).unwrap();
        std::fs::write(
            fixture.parent.state_path().join("data.json"),
            "workspace data",
        )
        .unwrap();
        git(
            &fixture.root,
            &["worktree", "lock", fixture.parent.path().to_str().unwrap()],
        );
        let lock = fixture.project.lock("test").unwrap();
        let meta = fixture.project.metadata().read().unwrap();
        let plan = plan_reparents(std::slice::from_ref(&fixture.parent), &meta).unwrap();
        let (removed_fields, reparented_fields) = removal_fields(
            &fixture.project,
            std::slice::from_ref(&fixture.parent),
            &meta,
            &plan,
        );
        let mut out = test_channel();

        assert!(
            remove_merged_source(
                &lock,
                &fixture.project,
                &fixture.parent,
                false,
                plan,
                removed_fields,
                reparented_fields,
                &mut out,
            )
            .is_err()
        );
        drop(lock);

        let metadata = fixture.project.metadata().read().unwrap();
        assert!(metadata.workspaces.contains_key("parent"));
        assert!(fixture.parent.state_path().join("data.json").exists());
    }

    #[test]
    fn merge_removal_clears_the_source_state_before_its_name_is_reused() {
        let fixture = removal_fixture();
        std::fs::create_dir_all(fixture.parent.state_path()).unwrap();
        std::fs::write(
            fixture.parent.state_path().join("data.json"),
            "workspace data",
        )
        .unwrap();
        let lock = fixture.project.lock("test").unwrap();
        let meta = fixture.project.metadata().read().unwrap();
        let plan = plan_reparents(std::slice::from_ref(&fixture.parent), &meta).unwrap();
        let (removed_fields, reparented_fields) = removal_fields(
            &fixture.project,
            std::slice::from_ref(&fixture.parent),
            &meta,
            &plan,
        );
        let mut out = test_channel();

        let (record, _) = remove_merged_source(
            &lock,
            &fixture.project,
            &fixture.parent,
            false,
            plan,
            removed_fields,
            reparented_fields,
            &mut out,
        )
        .unwrap();
        assert_eq!(record.workspace, "parent");
        drop(lock);

        assert!(!fixture.parent.state_path().exists());
        let metadata = fixture.project.metadata().read().unwrap();
        assert!(!metadata.workspaces.contains_key("parent"));
        git(
            &fixture.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "parent-reused",
                fixture.parent.path().to_str().unwrap(),
                "HEAD",
            ],
        );
        let reused = Workspace::new(fixture.project.clone(), "parent".to_string());
        std::fs::create_dir_all(reused.state_path()).unwrap();
        assert!(!reused.state_path().join("data.json").exists());
    }
}
