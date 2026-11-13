use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    iter,
    path::PathBuf,
    rc::Rc,
};

use crate::{
    error::{Error, Result},
    git::GitStatus,
    github::Gh,
    metadata::{ParentEdge, ProjectMeta, WorkspaceMeta},
    name::is_valid_name,
    project::{Project, ProjectLock},
    revision::{Head, Revision, Sha},
    tree::{self, Tree},
    worktree::{InProgressOperation, UpstreamStatus, Worktree},
};

/// Whether the stored anchor of a workspace still agrees with the tip of its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentStatus {
    /// The workspace has no parent.
    Root,
    /// The anchor is the parent tip, or an ancestor of it, which means the parent advanced.
    Current,
    /// The parent rewrote the history below the anchor. `dropped` holds the commits that the parent
    /// no longer carries and the anchor of the child still does.
    Rewritten {
        /// The commits of the `parent_tip..anchor` range.
        dropped: Vec<Sha>,
    },
}

/// How a workspace relates to its parent: the name, how far the stack of the child is behind and
/// ahead of the parent tip, whether the parent rewrote the history below the branch point, and
/// where a rebase starts its replay.
pub struct ParentData {
    /// The name of the parent workspace.
    pub name: String,
    /// The commits on the tip of the parent that the child does not have yet.
    pub commits_behind: usize,
    /// The commits on the child that the tip of the parent does not have yet.
    pub commits_ahead: usize,
    /// Whether the parent rewrote the history below the branch point.
    pub rewritten: bool,
    /// The stored anchor that a rebase replays from, while the anchor is still on the line of this
    /// workspace. It is `None` once the anchor moves off that line.
    /// A replay from a stale anchor fails instead of choosing another boundary.
    pub replay_base: Option<Sha>,
}

/// How a rebase runs. `interactive` opens `git rebase -i` and always replays the commits of the
/// workspace, even when the base tip already leads HEAD. `all` replays every commit ahead of the
/// base, not only those ahead of the branch point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebaseMode {
    /// Open the interactive todo editor, and always replay the commits of the workspace.
    pub interactive: bool,
    /// Replay the whole branch, and ignore the stored branch point.
    pub all: bool,
}

/// Runtime options for [`Workspace::rebase_onto`]: whether the replay is interactive or quiet, and
/// where its captured output goes in JSON mode.
#[derive(Default)]
pub struct RebaseIo<'a> {
    /// Open the interactive todo editor. `on_output` is not used in this case.
    pub interactive: bool,
    /// Pass `--quiet` to the replay.
    pub quiet: bool,
    /// When given, the replay is captured rather than inherited, and every stdout line reaches
    /// this sink.
    pub on_output: Option<&'a mut dyn FnMut(&str)>,
}

/// The decision that [`Workspace::plan_rebase`] makes: whether the rebase can skip the replay,
/// because the base tip already leads HEAD, or which branch point it replays from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebasePlan {
    /// The base tip already leads HEAD. Only the stored anchor needs an update.
    SkipReplay,
    /// Replay with `git rebase --onto <base> <branch_point>`, or with `<base>` alone when
    /// `branch_point` is `None`, which replays the whole branch.
    Replay {
        /// The branch point to replay from, or `None` to replay the whole branch.
        branch_point: Option<Sha>,
    },
}

/// The result of [`Workspace::rebase_onto`]: the rebase anchored the workspace onto the new parent
/// again, or an interactive replay stopped in the middle (`edit` or `break`), which leaves the new
/// anchor to a later call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The stored parent edge now points at the tip of `onto_ws`.
    Reanchored,
    /// The replay is still in progress. The stored anchor stays as it was.
    StoppedMidRebase,
}

/// A snapshot of the git status of a workspace, and of how its stack relates to its parent, when it
/// has one.
pub struct WorkspaceInfo {
    /// The file status of the index and the worktree.
    pub git_status: GitStatus,
    /// How the workspace relates to its parent, when it has one.
    pub parent: Option<ParentData>,
    /// How the branch of this workspace tracks its remote, when it tracks one.
    pub remote: Option<UpstreamStatus>,
    /// The git operation in progress, such as a rebase or a merge, when one runs.
    pub operation: Option<InProgressOperation>,
}

/// Classifies the anchor of a child against the tip of its parent, which the caller already
/// resolved. [`Workspace::parent_status`] and [`Workspace::info`] share it, so a caller that holds
/// both values does not read the head of the parent again.
fn classify_parent_status(
    worktree: &Worktree,
    base: &Sha,
    parent_tip: &impl Revision,
) -> Result<ParentStatus> {
    if worktree.is_ancestor(base, parent_tip)? {
        return Ok(ParentStatus::Current);
    }
    let dropped = worktree.commits_only_in(base, parent_tip)?;
    Ok(ParentStatus::Rewritten { dropped })
}

/// Validates a workspace name. Fails with [`Error::InvalidName`] unless the name starts with an
/// ASCII letter, digit, or underscore and continues with those characters or hyphens.
pub fn validate_name(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        return Err(Error::InvalidName(format!(
            "workspace name must match [A-Za-z0-9_][A-Za-z0-9_-]*: {name}"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct WorkspaceInner {
    project: Project,
    name: String,
}

/// A named worktree of a project.
#[derive(Debug, Clone)]
pub struct Workspace {
    inner: Rc<WorkspaceInner>,
}

impl PartialEq for Workspace {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) || *self.inner == *other.inner
    }
}

impl Eq for Workspace {}

impl Hash for Workspace {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.hash(state);
    }
}

impl Workspace {
    /// Creates a workspace from a project and a name. It does not check that the workspace exists.
    pub fn new(project: Project, name: String) -> Workspace {
        Workspace {
            inner: Rc::new(WorkspaceInner { project, name }),
        }
    }

    /// The project that this workspace belongs to.
    pub fn project(&self) -> &Project {
        &self.inner.project
    }

    /// The name of the workspace.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The qualified name, as `project/workspace`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.project().name(), self.name())
    }

    /// The filesystem path of the worktree of this workspace.
    pub fn path(&self) -> PathBuf {
        self.project().worktrees_path().join(self.name())
    }

    /// The state directory of this workspace, `<state>/workspaces/<name>/`.
    pub fn state_path(&self) -> PathBuf {
        self.project().workspaces_state_path().join(self.name())
    }

    /// Opens the git worktree for this workspace.
    pub fn worktree(&self) -> Result<Worktree> {
        let path = self.path();
        Worktree::get(&path).ok_or_else(|| Error::NotAWorktree(path.to_string_lossy().into_owned()))
    }

    /// The pull request number of the branch that this workspace has checked out. It is `None` when
    /// HEAD is detached, and `None` when the branch has no pull request.
    pub fn pr_number(&self) -> Result<Option<u64>> {
        let Some(branch) = self.worktree()?.get_branch() else {
            return Ok(None);
        };
        let dir = self.path();
        Gh::pr_number(Some(dir.as_path()), &branch)
    }

    /// The name of a new branch. It is `branch` when the caller gives one. Otherwise it is
    /// `<branch_prefix>/<name>` when the project has a `branch_prefix` setting, and the workspace
    /// name when it has none.
    fn derived_branch(&self, branch: Option<&str>) -> Result<String> {
        Ok(match branch {
            Some(b) => b.to_string(),
            None => match self.project().branch_prefix()?.as_deref() {
                Some(prefix) => format!("{prefix}/{}", self.name()),
                None => self.name().to_string(),
            },
        })
    }

    /// Creates this workspace as a new git worktree on a branch that forks from `from`.
    pub fn create_branch(
        &self,
        lock: &ProjectLock,
        from: &Worktree,
        branch: Option<&str>,
        git_ref: &impl Revision,
    ) -> Result<()> {
        from.create_branch(lock, &self.derived_branch(branch)?, &self.path(), git_ref)
    }

    /// Creates this workspace as a new git worktree whose new branch tracks the remote-tracking
    /// branch `remote_ref`, for example `origin/feature`.
    pub fn create_tracking_branch(
        &self,
        lock: &ProjectLock,
        from: &Worktree,
        branch: Option<&str>,
        remote_ref: &str,
    ) -> Result<()> {
        from.create_tracking_branch(
            lock,
            &self.derived_branch(branch)?,
            &self.path(),
            remote_ref,
        )
    }

    /// Creates this workspace as a new git worktree that checks out the existing branch `branch`.
    pub fn checkout_branch(&self, lock: &ProjectLock, from: &Worktree, branch: &str) -> Result<()> {
        from.checkout_branch(lock, branch, &self.path())
    }

    /// Creates this workspace as a new git worktree with a detached HEAD at `git_ref`.
    pub fn checkout_detached(
        &self,
        lock: &ProjectLock,
        from: &Worktree,
        git_ref: &impl Revision,
    ) -> Result<()> {
        from.checkout_detached(lock, &self.path(), git_ref)
    }

    /// The parent edge from the metadata, which holds the name and the anchor base. It is `None`
    /// when the workspace has no parent.
    pub fn parent_edge(&self, meta: &ProjectMeta) -> Option<ParentEdge> {
        meta.workspaces
            .get(self.name())
            .and_then(|ws| ws.parent.clone())
    }

    /// The name of the parent workspace from the metadata, when the workspace has a parent.
    pub fn parent_name(&self, meta: &ProjectMeta) -> Option<String> {
        self.parent_edge(meta).map(|parent| parent.name)
    }

    /// The parent workspace, when it exists on disk.
    pub fn parent_workspace(&self, meta: &ProjectMeta) -> Option<Workspace> {
        self.parent_name(meta)
            .and_then(|name| Workspace::get(self.project(), &name))
    }

    /// Classifies whether the stored anchor of this workspace still agrees with the current tip of
    /// its parent. See [`ParentStatus`].
    pub fn parent_status(&self, meta: &ProjectMeta) -> Result<ParentStatus> {
        let parent = match self.parent_edge(meta) {
            Some(parent) => parent,
            None => return Ok(ParentStatus::Root),
        };
        let parent_workspace = match Workspace::get(self.project(), &parent.name) {
            Some(workspace) => workspace,
            None => {
                return Err(Error::DanglingParent {
                    workspace: self.name().to_string(),
                    parent: parent.name,
                });
            }
        };
        let parent_tip = parent_workspace.worktree()?.head()?;
        let worktree = self.worktree()?;
        classify_parent_status(&worktree, &parent.base, &parent_tip)
    }

    /// Selects the branch point that a rebase replays the commits of this workspace from.
    ///
    /// With `all`, the rebase replays the whole branch, so there is no branch point.
    /// Otherwise, it uses the stored anchor when that anchor is an ancestor of HEAD.
    /// A stale anchor fails with [`Error::StaleRebaseAnchor`] instead of choosing another boundary.
    pub fn replay_branch_point(
        &self,
        worktree: &Worktree,
        meta: &ProjectMeta,
        head: &impl Revision,
        all: bool,
    ) -> Result<Option<Sha>> {
        if all {
            return Ok(None);
        }
        match self.parent_edge(meta) {
            Some(parent) if !worktree.is_ancestor(&parent.base, head)? => {
                Err(Error::StaleRebaseAnchor {
                    workspace: self.name().to_string(),
                    anchor: parent.base,
                })
            }
            Some(parent) => Ok(Some(parent.base)),
            None => Ok(None),
        }
    }

    /// Plans a rebase of this workspace onto the tip of `onto_ws`, which is `onto_head`.
    ///
    /// It blocks `all` onto a stored parent that rewrote its history, because that replays the
    /// dropped commits of the parent as duplicates. When the parent rewrote nothing, `all` onto it
    /// does what the default does, so the block costs nothing.
    ///
    /// When the base tip leads HEAD and includes the stored fork point, the plan skips the replay.
    /// It also skips when the stored fork point is stale.
    /// A base below a valid fork point needs a replay to exclude inherited commits.
    /// Under `interactive` it still replays the commits of the workspace, so `git rebase -i` can
    /// edit them. See [`Workspace::replay_branch_point`] for how it picks the branch point.
    pub fn plan_rebase(
        &self,
        worktree: &Worktree,
        meta: &ProjectMeta,
        onto_ws: &Workspace,
        onto_head: &Head,
        head: &Head,
        mode: RebaseMode,
    ) -> Result<RebasePlan> {
        if self == onto_ws || onto_ws.has_ancestor(self, meta)? {
            return Err(Error::CycleDetected(self.name().to_string()));
        }
        if mode.all && self.parent_workspace(meta).as_ref() == Some(onto_ws) {
            self.require_chain_current(onto_ws, meta)?;
        }
        if !mode.interactive && worktree.is_ancestor(onto_head, head)? {
            let inherited = match self.parent_edge(meta) {
                Some(edge) if !mode.all => {
                    !worktree.is_ancestor(&edge.base, head)?
                        || worktree.is_ancestor(&edge.base, onto_head)?
                }
                _ => true,
            };
            if inherited {
                return Ok(RebasePlan::SkipReplay);
            }
        }
        let branch_point = self.replay_branch_point(worktree, meta, head, mode.all)?;
        Ok(RebasePlan::Replay { branch_point })
    }

    /// Runs `plan` against the worktree of this workspace, then anchors this workspace onto the
    /// current tip of `onto_ws` in the project metadata. It anchors only after the run succeeds. A
    /// [`RebasePlan::SkipReplay`] plan runs nothing, which covers a conflicted rebase that a person
    /// already finished by hand with `git rebase --continue`.
    ///
    /// A replay that fails returns its error and leaves the stored anchor as it was. An interactive
    /// replay that stops in the middle, at an `edit` or a `break` of the todo list, exits 0 with
    /// the rebase still in progress. That case skips the anchor too, and the stored anchor stays as
    /// it was until a later `subspace rebase` finishes the replay and anchors it through the
    /// `SkipReplay` path.
    /// `io.on_output`, when given, runs the replay through [`Worktree::rebase_captured`] instead
    /// of [`Worktree::rebase`] and passes it every captured stdout line, in JSON mode where
    /// inherited stdout would print ahead of the machine document. It is not used for an
    /// interactive rebase, which needs the terminal.
    pub fn rebase_onto(
        &self,
        lock: &ProjectLock,
        onto_ws: &Workspace,
        onto_head: &Head,
        plan: &RebasePlan,
        io: RebaseIo,
    ) -> Result<RebaseOutcome> {
        if let RebasePlan::Replay { branch_point } = plan {
            let worktree = self.worktree()?;
            let onto_sha = onto_head.sha();
            let branch_point = branch_point.as_ref().map(|sha| sha.sha());
            let mut rebase_args = vec!["--empty=drop"];
            if io.interactive {
                rebase_args.push("--interactive");
            }
            if io.quiet {
                rebase_args.push("--quiet");
            }
            // Leave out the `<branch>` positional. Git checks out an explicit `<branch>` first, and
            // a `HEAD` there resolves to a commit and detaches HEAD. Without it, git rebases
            // `<branch_point>..HEAD` in place, and the workspace stays on its branch.
            match branch_point.as_deref() {
                Some(branch_point) => {
                    rebase_args.extend(["--onto", &onto_sha, branch_point]);
                }
                None => rebase_args.push(&onto_sha),
            }
            match io.on_output {
                Some(sink) => {
                    let run = worktree.rebase_captured(lock, &rebase_args)?;
                    for line in run.stdout.lines() {
                        sink(line);
                    }
                    crate::worktree::rebase_status_result(run.status)?;
                }
                None => worktree.rebase(lock, &rebase_args)?,
            }

            if matches!(
                worktree.operation_in_progress(),
                Some(InProgressOperation::Rebase { .. })
            ) {
                return Ok(RebaseOutcome::StoppedMidRebase);
            }
        }

        let parent_edge = ParentEdge {
            name: onto_ws.name().to_string(),
            base: onto_head.commit(),
        };
        self.project().metadata().update(lock, |m| {
            m.workspaces
                .entry(self.name().to_string())
                .or_insert_with(WorkspaceMeta::default)
                .parent = Some(parent_edge);
            Ok(())
        })?;
        Ok(RebaseOutcome::Reanchored)
    }

    /// Walks up the parent chain and returns the root workspace.
    pub fn root_workspace(&self, meta: &ProjectMeta) -> Result<Workspace> {
        tree::find_root(self.clone(), |ws| ws.parent_workspace(meta))
    }

    /// The direct child workspaces of this workspace.
    pub fn children(&self, meta: &ProjectMeta) -> Result<Vec<Workspace>> {
        Ok(self
            .project()
            .workspaces()?
            .into_iter()
            .filter(|ws| ws.parent_name(meta).as_deref() == Some(self.name()))
            .collect())
    }

    /// The sibling workspaces, which share the parent of this workspace and are not this workspace.
    /// It is empty for a root.
    pub fn siblings(&self, meta: &ProjectMeta) -> Result<Vec<Workspace>> {
        let parent = match self.parent_name(meta) {
            Some(parent) => parent,
            None => return Ok(vec![]),
        };
        Ok(self
            .project()
            .workspaces()?
            .into_iter()
            .filter(|ws| {
                ws.parent_name(meta).as_deref() == Some(parent.as_str()) && ws.name() != self.name()
            })
            .collect())
    }

    /// Takes a snapshot of the git status of this workspace, and reads from `meta` how its stack
    /// relates to its parent, which is the commit counts and the rewrite state.
    pub fn info(&self, meta: &ProjectMeta) -> Result<WorkspaceInfo> {
        let worktree = self.worktree()?;
        let head = worktree.head()?;
        let git_status = worktree.status()?;
        let remote = worktree.upstream_status()?;
        let operation = worktree.operation_in_progress();

        let parent = match self
            .parent_edge(meta)
            .and_then(|edge| Workspace::get(self.project(), &edge.name).map(|ws| (edge, ws)))
        {
            Some((edge, parent_workspace)) => {
                let parent_worktree = parent_workspace.worktree()?;
                let parent_head = parent_worktree.head()?;
                let commits_behind = parent_worktree.count_unique_revisions(&head, &parent_head)?;
                let commits_ahead = parent_worktree.count_unique_revisions(&parent_head, &head)?;
                let rewritten = matches!(
                    classify_parent_status(&worktree, &edge.base, &parent_head)?,
                    ParentStatus::Rewritten { .. }
                );
                let replay_base = worktree
                    .is_ancestor(&edge.base, &head)?
                    .then_some(edge.base);
                Some(ParentData {
                    name: parent_workspace.name().to_string(),
                    commits_behind,
                    commits_ahead,
                    rewritten,
                    replay_base,
                })
            }
            None => None,
        };

        Ok(WorkspaceInfo {
            git_status,
            parent,
            remote,
            operation,
        })
    }

    /// The branch point off this workspace of a child stack whose root is `tip`. It is
    /// `merge-base(self_tip, tip)`.
    pub fn branch_base(&self, tip: &impl Revision) -> Result<Sha> {
        let worktree = self.worktree()?;
        let parent_tip = worktree.head()?;
        worktree.merge_base(&parent_tip, tip)
    }

    /// The [`ParentEdge`] that anchors a child at `tip` onto this workspace. It is `{ name:
    /// self.name(), base: self.branch_base(tip)? }`.
    pub fn edge_for_child(&self, tip: &impl Revision) -> Result<ParentEdge> {
        Ok(ParentEdge {
            name: self.name().to_string(),
            base: self.branch_base(tip)?,
        })
    }

    /// Walks the ancestor chain of this workspace, from its direct parent up to the root, and
    /// yields each parent workspace. Past a sane depth it fails with [`Error::CycleDetected`],
    /// which guards against a cycle in corrupt metadata.
    pub fn ancestors<'a>(&self, meta: &'a ProjectMeta) -> Ancestors<'a> {
        Ancestors {
            current: Some(self.clone()),
            meta,
            depth: 0,
        }
    }

    /// Whether `other` is in the ancestor chain of this workspace, which is `self.ancestors(meta)`.
    /// An error of the walk, such as a dangling parent or a cycle, comes back as an error, not as
    /// `false`.
    pub fn has_ancestor(&self, other: &Workspace, meta: &ProjectMeta) -> Result<bool> {
        for ancestor in self.ancestors(meta) {
            if ancestor? == *other {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Yields this workspace, then each of its ancestors, from its direct parent up to the root.
    fn self_and_ancestors<'a>(
        &self,
        meta: &'a ProjectMeta,
    ) -> impl Iterator<Item = Result<Workspace>> + 'a {
        iter::once(Ok(self.clone())).chain(self.ancestors(meta))
    }

    /// Walks the chain of this workspace upward from its own edge, and returns the first workspace
    /// whose parent rewrote the history below its anchor, with the commits that the parent dropped.
    ///
    /// The walk stops at `bound` and returns `None`, and it does not check the edge of `bound`. A
    /// `bound` of `None` walks to the root. It returns `None` when it finds no rewritten edge
    /// before it stops.
    pub fn first_rewritten_edge(
        &self,
        meta: &ProjectMeta,
        bound: Option<&Workspace>,
    ) -> Result<Option<(Workspace, Vec<Sha>)>> {
        for node in self.self_and_ancestors(meta) {
            let node = node?;
            if bound == Some(&node) {
                return Ok(None);
            }
            match node.parent_status(meta)? {
                ParentStatus::Rewritten { dropped } => return Ok(Some((node, dropped))),
                ParentStatus::Root => return Ok(None),
                ParentStatus::Current => {}
            }
        }
        Ok(None)
    }

    /// Fails with [`Error::ParentRewritten`] when an edge of the chain from this workspace up to
    /// `ancestor` rewrote the history below its anchor. It does not check the edge of `ancestor`,
    /// because the walk stops there.
    pub fn require_chain_current(&self, ancestor: &Workspace, meta: &ProjectMeta) -> Result<()> {
        match self.first_rewritten_edge(meta, Some(ancestor))? {
            Some((node, dropped)) => Err(Error::ParentRewritten {
                workspace: node.name().to_string(),
                dropped,
            }),
            None => Ok(()),
        }
    }

    /// Resolves a sigil string, such as `@parent`, `@root`, or `@current`, against this workspace.
    pub fn resolve_sigil(&self, sigil: &str) -> Result<Workspace> {
        crate::scope::Sigil::parse(sigil)
            .ok_or_else(|| Error::UnknownSigil(sigil.to_string()))?
            .resolve(self)
    }

    /// Returns the branch that the metadata records for this workspace. It fails when the
    /// metadata records none. It never reads the checked-out branch.
    pub fn recorded_branch(&self, meta: &ProjectMeta) -> Result<String> {
        meta.workspaces
            .get(self.name())
            .and_then(|entry| entry.branch.clone())
            .ok_or_else(|| Error::NoRecordedBranch(self.full_name()))
    }

    /// Looks up a workspace by name.
    /// Returns `None` when the name is invalid or its directory does not exist.
    pub fn get(project: &Project, name: &str) -> Option<Workspace> {
        if validate_name(name).is_err() {
            return None;
        }
        let ws = Workspace::new(project.clone(), name.to_string());
        if ws.path().is_dir() { Some(ws) } else { None }
    }
}

/// An iterator over the ancestor chain of a workspace, from [`Workspace::ancestors`]. It yields one
/// parent workspace after another, and stops at the root.
///
/// It yields one [`Error::DanglingParent`] when the metadata names a parent that is not on disk,
/// and one [`Error::CycleDetected`] when the chain goes past a sane depth.
pub struct Ancestors<'a> {
    current: Option<Workspace>,
    meta: &'a ProjectMeta,
    depth: usize,
}

impl Iterator for Ancestors<'_> {
    type Item = Result<Workspace>;

    fn next(&mut self) -> Option<Result<Workspace>> {
        let workspace = self.current.take()?;
        let edge = workspace.parent_edge(self.meta)?;
        let parent = match Workspace::get(workspace.project(), &edge.name) {
            Some(parent) => parent,
            None => {
                return Some(Err(Error::DanglingParent {
                    workspace: workspace.name().to_string(),
                    parent: edge.name,
                }));
            }
        };
        self.depth += 1;
        if self.depth >= 10_000 {
            return Some(Err(Error::CycleDetected(parent.name().to_string())));
        }
        self.current = Some(parent.clone());
        Some(Ok(parent))
    }
}

/// The new parent edge that a child workspace gets after its parent is removed. It pairs the name
/// of the child with its new edge, which is `None` when the child becomes a root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reparent {
    /// The name of the child workspace that gets a new parent.
    child: String,
    /// The new parent edge of the child, or `None` when it becomes a root.
    edge: Option<ParentEdge>,
}

/// A plan of parent-edge changes for the children that outlive the removed workspaces.
/// [`plan_reparents`] builds it, and [`ReparentPlan::apply`] writes it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReparentPlan {
    reparents: Vec<Reparent>,
}

impl ReparentPlan {
    /// Applies each planned edge to `meta`, and sets the parent edge of the workspace it names. It
    /// skips a name that the metadata does not hold.
    pub fn apply(&self, meta: &mut ProjectMeta) {
        for reparent in &self.reparents {
            if let Some(ws_meta) = meta.workspaces.get_mut(&reparent.child) {
                ws_meta.parent = reparent.edge.clone();
            }
        }
    }

    /// Walks the children that get a new parent, as `(child, new parent)` pairs. The new parent is
    /// `None` when the child becomes a root.
    pub fn reparented(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.reparents
            .iter()
            .map(|r| (r.child.as_str(), r.edge.as_ref().map(|e| e.name.as_str())))
    }

    /// Whether the plan gives no workspace a new parent.
    pub fn is_empty(&self) -> bool {
        self.reparents.is_empty()
    }
}

/// A repair plan for the workspace metadata of a project, which is `workspaces.json`. It holds the
/// dead workspace entries to drop, whose worktree directory is gone, the new parent of each child
/// that outlives them, which is a live ancestor, and a stale default to clear. [`plan_repair`]
/// builds it, and [`RepairPlan::apply`] writes it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPlan {
    pruned: Vec<String>,
    reparents: ReparentPlan,
    stale_default: Option<String>,
}

impl RepairPlan {
    /// The names of the dead workspace metadata entries that the plan removes, in order.
    pub fn pruned(&self) -> &[String] {
        &self.pruned
    }

    /// The stale default workspace name that the plan clears, when there is one.
    pub fn stale_default(&self) -> Option<&str> {
        self.stale_default.as_deref()
    }

    /// Walks the children that get a new parent, as `(child, new parent)` pairs. The new parent is
    /// `None` when the child becomes a root.
    pub fn reparented(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.reparents.reparented()
    }

    /// Whether the plan changes any metadata.
    pub fn is_empty(&self) -> bool {
        self.pruned.is_empty() && self.reparents.is_empty() && self.stale_default.is_none()
    }

    /// Gives each child that outlives its parent a new parent, removes the dead entries, and clears
    /// a stale default. The new parents come first, so it reads the edge of a pruned entry before
    /// it drops that entry.
    pub fn apply(&self, meta: &mut ProjectMeta) {
        self.reparents.apply(meta);
        for name in &self.pruned {
            meta.workspaces.remove(name);
        }
        if self.stale_default.is_some() {
            meta.default = None;
        }
    }
}

/// A worktree link that a repair can rebuild. It holds the workspace whose administrative entry is
/// gone, and the branch that its rebuilt HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relink {
    /// The name of the workspace whose link the repair rebuilds.
    pub workspace: String,
    /// The branch that the metadata records for the workspace.
    pub branch: String,
}

/// Why a broken worktree link cannot be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrepairable {
    /// The directory holds no `.git` file, so it never was a linked worktree.
    NotAWorktree,
    /// The metadata records no branch for the workspace.
    NoBranch,
    /// The branch of the record does not exist in the repository.
    MissingBranch(String),
    /// Another worktree has the branch of the record checked out.
    BranchInUse {
        /// The branch of the record.
        branch: String,
        /// The worktree that has the branch checked out.
        holder: String,
    },
    /// The rebuild ran and failed. It carries the error that stopped it.
    RebuildFailed(String),
}

/// A plan that rebuilds the worktree links whose administrative entry is gone. Such a workspace has
/// a directory, but git no longer opens it as a worktree, and the repository holds no entry for
/// `git worktree repair` to correct.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelinkPlan {
    relinks: Vec<Relink>,
    unrepairable: Vec<(String, Unrepairable)>,
}

impl RelinkPlan {
    /// The links that the plan rebuilds, in workspace order.
    pub fn relinks(&self) -> &[Relink] {
        &self.relinks
    }

    /// The broken links that the plan cannot rebuild, each with its reason, in workspace order.
    pub fn unrepairable(&self) -> &[(String, Unrepairable)] {
        &self.unrepairable
    }

    /// Whether the plan found no broken links at all.
    pub fn is_empty(&self) -> bool {
        self.relinks.is_empty() && self.unrepairable.is_empty()
    }
}

/// The name of the administrative entry of a workspace. It comes from the `.git` file of the
/// workspace, with the workspace name as the fallback when nothing can read that file. The two
/// names differ after a rename of the workspace, which moves the directory and leaves the entry
/// under its first name.
fn entry_name(ws: &Workspace) -> String {
    crate::worktree::linked_entry_name(&ws.path()).unwrap_or_else(|| ws.name().to_string())
}

/// Plans the worktree-link rebuilds for `project` against `meta`, and reads the repository through
/// `anchor`.
///
/// A workspace qualifies when its `.git` file exists, git cannot open it, and the repository holds
/// no administrative entry under the name that the file records. `git worktree prune` leaves that
/// state behind when it collects the entries of a project that someone moved by hand.
///
/// The target of a rebuild is the branch that the metadata records. A workspace is unrepairable
/// when it has no `.git` file, when the metadata records no branch for it, when the repository has
/// no such branch, or when another worktree has that branch checked out. When two workspaces record
/// one branch, the plan rebuilds the first, and reports the branch as in use for the second.
pub fn plan_relinks(
    project: &Project,
    meta: &ProjectMeta,
    anchor: &Worktree,
) -> Result<RelinkPlan> {
    let mut plan = RelinkPlan::default();
    let mut claimed: HashMap<String, String> = HashMap::new();
    for ws in project.workspaces()? {
        let dot_git = ws.path().join(".git");
        if ws.worktree().is_ok() || dot_git.is_dir() {
            continue;
        }
        if !dot_git.is_file() {
            plan.unrepairable
                .push((ws.name().to_string(), Unrepairable::NotAWorktree));
            continue;
        }
        if anchor.has_worktree_link(&entry_name(&ws)) {
            continue;
        }
        match meta
            .workspaces
            .get(ws.name())
            .and_then(|m| m.branch.clone())
        {
            None => plan
                .unrepairable
                .push((ws.name().to_string(), Unrepairable::NoBranch)),
            Some(branch) if !anchor.has_branch(&branch) => plan
                .unrepairable
                .push((ws.name().to_string(), Unrepairable::MissingBranch(branch))),
            Some(branch) => match anchor
                .branch_holder(&branch)
                .or_else(|| claimed.get(&branch).cloned())
            {
                Some(holder) => plan.unrepairable.push((
                    ws.name().to_string(),
                    Unrepairable::BranchInUse { branch, holder },
                )),
                None => {
                    claimed.insert(branch.clone(), ws.name().to_string());
                    plan.relinks.push(Relink {
                        workspace: ws.name().to_string(),
                        branch,
                    });
                }
            },
        }
    }
    Ok(plan)
}

/// Applies `plan` and repairs the worktree links of the project, in the order that the operations
/// need: it rebuilds the deleted entries, it rewrites the stale links with `git worktree repair`,
/// and it prunes the dead entries last. An earlier prune collects the stale entries that the
/// repair step needs.
///
/// A rebuild that fails moves its workspace onto the unrepairable list, and the pass continues.
/// Returns the plan as it applied it, with both lists in workspace order.
pub fn apply_relinks(
    project: &Project,
    plan: RelinkPlan,
    anchor: &Worktree,
    lock: &ProjectLock,
) -> Result<RelinkPlan> {
    let mut applied = RelinkPlan {
        relinks: Vec::new(),
        unrepairable: plan.unrepairable,
    };
    for relink in plan.relinks {
        let path = project.worktrees_path().join(&relink.workspace);
        match anchor.relink_worktree(lock, &path, &relink.branch) {
            Ok(()) => applied.relinks.push(relink),
            Err(e) => applied
                .unrepairable
                .push((relink.workspace, Unrepairable::RebuildFailed(e.to_string()))),
        }
    }
    let paths: Vec<PathBuf> = project
        .workspaces()?
        .iter()
        .filter(|ws| {
            ws.worktree().is_ok()
                || (ws.path().join(".git").exists() && anchor.has_worktree_link(&entry_name(ws)))
        })
        .map(|ws| ws.path())
        .collect();
    anchor.repair_paths(lock, &paths)?;
    anchor.prune_worktrees(lock)?;
    applied.unrepairable.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(applied)
}

/// Plans the repairs to the workspace metadata of `project` against the worktrees on disk. It drops
/// an entry whose directory is gone. It anchors each child that outlives such an entry onto the
/// nearest live ancestor, or makes that child a root. It clears the default when the default names
/// a workspace that is gone.
///
/// A parent reference that names no live workspace counts as dead too, so the children of that
/// parent become roots.
pub fn plan_repair(project: &Project, meta: &ProjectMeta) -> Result<RepairPlan> {
    let live: HashSet<String> = project
        .workspaces()?
        .into_iter()
        .map(|ws| ws.name().to_string())
        .collect();

    let mut dead: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for name in meta.workspaces.keys() {
        if !live.contains(name) && seen.insert(name.clone()) {
            dead.push(name.clone());
        }
    }
    for ws_meta in meta.workspaces.values() {
        if let Some(parent) = &ws_meta.parent
            && !live.contains(&parent.name)
            && seen.insert(parent.name.clone())
        {
            dead.push(parent.name.clone());
        }
    }
    dead.sort();

    let targets: Vec<Workspace> = dead
        .iter()
        .map(|name| Workspace::new(project.clone(), name.clone()))
        .collect();
    let reparents = plan_reparents(&targets, meta)?;

    let pruned: Vec<String> = dead
        .into_iter()
        .filter(|name| meta.workspaces.contains_key(name))
        .collect();

    let stale_default = meta
        .default
        .as_ref()
        .filter(|name| !live.contains(name.as_str()))
        .cloned();

    Ok(RepairPlan {
        pruned,
        reparents,
        stale_default,
    })
}

/// Plans the new parent edges for the children that outlive `targets`, which are the workspaces
/// about to go. It anchors each child onto the nearest ancestor outside `targets`.
///
/// A child whose direct parent is a target, and whose grandparent or a more distant ancestor is a
/// target too, walks up through each removed ancestor in turn. At each hop the anchor becomes
/// whichever of the stored base of that hop and the anchor so far is the ancestor of the other. So
/// this plan never computes a live merge base again, which a rewrite can corrupt.
///
/// It skips a child that is in `targets`, because that child goes too. A child of a removed root
/// becomes a root.
pub fn plan_reparents(targets: &[Workspace], meta: &ProjectMeta) -> Result<ReparentPlan> {
    let target_names: HashSet<&str> = targets.iter().map(|ws| ws.name()).collect();
    let mut reparents = Vec::new();
    for ws in targets {
        for child in ws.children(meta)? {
            if target_names.contains(child.name()) {
                continue;
            }
            let edge = match child.parent_edge(meta) {
                Some(child_edge) => {
                    reparent_edge(&child, ws, child_edge.base, &target_names, meta)?
                }
                None => None,
            };
            reparents.push(Reparent {
                child: child.name().to_string(),
                edge,
            });
        }
    }
    Ok(ReparentPlan { reparents })
}

/// Walks up from `removed_parent` through the workspaces that `target_names` holds, one after
/// another, and carries the anchor of `child` forward at each hop. It stops at the first ancestor
/// outside `target_names`, which it returns as the new edge, or at the root, where it returns
/// `None` and `child` becomes a root.
fn reparent_edge(
    child: &Workspace,
    removed_parent: &Workspace,
    child_edge_base: Sha,
    target_names: &HashSet<&str>,
    meta: &ProjectMeta,
) -> Result<Option<ParentEdge>> {
    let worktree = child.worktree()?;
    let mut base = child_edge_base;
    let mut node = removed_parent.clone();
    loop {
        let Some(edge) = node.parent_edge(meta) else {
            return Ok(None);
        };
        base = if worktree.is_ancestor(&edge.base, &base)? {
            edge.base
        } else {
            base
        };
        if !target_names.contains(edge.name.as_str()) {
            return Ok(Some(ParentEdge {
                name: edge.name,
                base,
            }));
        }
        match Workspace::get(child.project(), &edge.name) {
            Some(next) => node = next,
            None => return Ok(None),
        }
    }
}

/// Plans the parent edges that come from an insert of `new_ws` between `child` and its former
/// parent.
///
/// `new_ws` takes the former parent, when there is one, through a new [`Workspace::branch_base`]
/// anchor off the tip of `new_ws`. `child` anchors onto `new_ws` the same way, off the current tip
/// of `child`.
///
/// `former_parent_edge` is the edge of `child` into its former parent, as it was before the insert.
/// It is `None` for an insert above a root, and then `new_ws` becomes a root too. Returns the edge
/// of `new_ws` and the new edge of `child`.
pub fn plan_insert_parent(
    child: &Workspace,
    former_parent_edge: Option<&ParentEdge>,
    new_ws: &Workspace,
) -> Result<(Option<ParentEdge>, ParentEdge)> {
    let former_parent =
        former_parent_edge.and_then(|edge| Workspace::get(child.project(), &edge.name));
    let new_ws_edge = match former_parent {
        Some(former_parent) => {
            let new_ws_tip = new_ws.worktree()?.head()?;
            Some(former_parent.edge_for_child(&new_ws_tip)?)
        }
        None => None,
    };
    let child_head = child.worktree()?.head()?;
    let child_edge = new_ws.edge_for_child(&child_head)?;
    Ok((new_ws_edge, child_edge))
}

/// The nearest workspace on the parent chain of `a` and on the parent chain of `b`. It is `None`
/// when the two belong to separate trees.
pub fn nearest_common_ancestor(
    a: &Workspace,
    b: &Workspace,
    meta: &ProjectMeta,
) -> Result<Option<Workspace>> {
    let a_chain: Vec<Workspace> = a.self_and_ancestors(meta).collect::<Result<_>>()?;
    for node in b.self_and_ancestors(meta) {
        let node = node?;
        if a_chain.contains(&node) {
            return Ok(Some(node));
        }
    }
    Ok(None)
}

/// Fails with [`Error::ParentRewritten`] when the chain of `dest` or the chain of `source` carries
/// a rewritten edge below their nearest common ancestor. It does nothing when the two workspaces
/// belong to separate trees.
pub fn require_merge_chains_current(
    dest: &Workspace,
    source: &Workspace,
    meta: &ProjectMeta,
) -> Result<()> {
    if let Some(ancestor) = nearest_common_ancestor(dest, source, meta)? {
        dest.require_chain_current(&ancestor, meta)?;
        source.require_chain_current(&ancestor, meta)?;
    }
    Ok(())
}

/// A subtree of workspaces of a project, with a root.
pub struct WorkspaceTree {
    root_index: usize,
    workspaces: Vec<Workspace>,
    index: HashMap<String, usize>,
    parent_of: HashMap<usize, usize>,
    children_of: HashMap<usize, Vec<usize>>,
}

impl WorkspaceTree {
    /// Builds the tree that holds the workspace and every relative of it, which is the whole tree
    /// under its root ancestor. It reads the worktrees and the metadata from disk.
    pub fn from_workspace(workspace: &Workspace) -> Result<WorkspaceTree> {
        let project = workspace.project();
        let all_workspaces = project.workspaces()?;
        let names: HashSet<&str> = all_workspaces.iter().map(|ws| ws.name()).collect();
        let edges = project.metadata().read()?.edges(&names);
        let root_name = tree::find_root(workspace.name().to_string(), |name| {
            edges.get(name.as_str()).cloned()
        })?;
        Ok(WorkspaceTree::build(&root_name, &all_workspaces, &edges))
    }

    /// Every workspace tree of the project.
    pub fn roots(project: &Project) -> Result<Vec<WorkspaceTree>> {
        let workspaces = project.workspaces()?;
        let names: HashSet<&str> = workspaces.iter().map(|ws| ws.name()).collect();
        let edges = project.metadata().read()?.edges(&names);

        let mut assigned: HashSet<String> = HashSet::new();
        let mut trees = Vec::new();

        for ws in &workspaces {
            if edges.contains_key(ws.name()) || assigned.contains(ws.name()) {
                continue;
            }
            let tree = WorkspaceTree::build(ws.name(), &workspaces, &edges);
            for node in tree.nodes() {
                assigned.insert(node.name().to_string());
            }
            trees.push(tree);
        }
        Ok(trees)
    }

    /// Every workspace of this tree.
    pub fn nodes(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// The name of the parent workspace inside this tree, when the workspace has one.
    pub fn parent_name(&self, workspace: &Workspace) -> Option<&str> {
        let &i = self.index.get(workspace.name())?;
        self.parent_of.get(&i).map(|&pi| self.workspaces[pi].name())
    }

    /// The subtree under `workspace`, in post-order, with each child before its parent.
    pub fn post_order(&self, workspace: &Workspace) -> Vec<Workspace> {
        let mut result = Vec::new();
        if let Some(&i) = self.index.get(workspace.name()) {
            self.post_order_at(i, &mut result);
        }
        result
    }

    fn post_order_at(&self, i: usize, result: &mut Vec<Workspace>) {
        if let Some(children) = self.children_of.get(&i) {
            for &ci in children {
                self.post_order_at(ci, result);
            }
        }
        result.push(self.workspaces[i].clone());
    }

    /// The subtree under `workspace`, in pre-order, with each parent before its children.
    pub fn pre_order(&self, workspace: &Workspace) -> Vec<Workspace> {
        let mut result = Vec::new();
        if let Some(&i) = self.index.get(workspace.name()) {
            self.pre_order_at(i, &mut result);
        }
        result
    }

    fn pre_order_at(&self, i: usize, result: &mut Vec<Workspace>) {
        result.push(self.workspaces[i].clone());
        if let Some(children) = self.children_of.get(&i) {
            for &ci in children {
                self.pre_order_at(ci, result);
            }
        }
    }

    fn build(
        root_name: &str,
        all_workspaces: &[Workspace],
        edges: &HashMap<String, String>,
    ) -> WorkspaceTree {
        let mut children_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        for (child, parent) in edges {
            children_by_name
                .entry(parent.as_str())
                .or_default()
                .push(child.as_str());
        }

        let mut members: HashSet<&str> = HashSet::new();
        members.insert(root_name);
        let mut queue = VecDeque::new();
        queue.push_back(root_name);
        while let Some(name) = queue.pop_front() {
            if let Some(children) = children_by_name.get(name) {
                for &child in children {
                    if members.insert(child) {
                        queue.push_back(child);
                    }
                }
            }
        }

        let workspaces: Vec<Workspace> = all_workspaces
            .iter()
            .filter(|ws| members.contains(ws.name()))
            .cloned()
            .collect();

        let index: HashMap<String, usize> = workspaces
            .iter()
            .enumerate()
            .map(|(i, ws)| (ws.name().to_string(), i))
            .collect();

        let root_index = index[root_name];

        let mut parent_of = HashMap::new();
        let mut children_of: HashMap<usize, Vec<usize>> = HashMap::new();
        for (child_name, parent_name) in edges {
            if let (Some(&ci), Some(&pi)) = (
                index.get(child_name.as_str()),
                index.get(parent_name.as_str()),
            ) {
                parent_of.insert(ci, pi);
                children_of.entry(pi).or_default().push(ci);
            }
        }

        WorkspaceTree {
            root_index,
            workspaces,
            index,
            parent_of,
            children_of,
        }
    }
}

impl Tree for WorkspaceTree {
    type Node = Workspace;

    fn root(&self) -> &Workspace {
        &self.workspaces[self.root_index]
    }

    fn parent(&self, node: &Workspace) -> Option<&Workspace> {
        let &i = self.index.get(node.name())?;
        self.parent_of.get(&i).map(|&pi| &self.workspaces[pi])
    }

    fn children(&self, node: &Workspace) -> Vec<&Workspace> {
        let Some(&i) = self.index.get(node.name()) else {
            return Vec::new();
        };
        self.children_of
            .get(&i)
            .map(|indices| indices.iter().map(|&ci| &self.workspaces[ci]).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitFileStatus;
    use crate::metadata::WorkspaceMeta;
    use crate::test_support::{commit, git, rev};
    use std::path::Path;

    /// Builds a project whose `parent` workspace holds two commits, and whose `child` workspace
    /// forks from the parent tip and adds a commit of its own. The two worktrees share one object
    /// store, so the child can resolve the commits of the parent. Returns the setup and the fork
    /// commit that the child anchors at.
    fn parent_child() -> (tempfile::TempDir, Workspace, Workspace, Sha) {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let worktrees = project.worktrees_path();
        std::fs::create_dir_all(&worktrees).unwrap();

        let parent_path = worktrees.join("parent");
        std::fs::create_dir(&parent_path).unwrap();
        git(&parent_path, &["init", "-q"]);
        commit(&parent_path, "a\n", "initial");
        commit(&parent_path, "a\nb\n", "second");
        let fork = rev(&parent_path, "HEAD");

        let child_path = worktrees.join("child");
        git(
            &parent_path,
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
        commit(&child_path, "a\nb\nchild\n", "child work");

        let parent_ws = Workspace::new(project.clone(), "parent".to_string());
        let child_ws = Workspace::new(project, "child".to_string());
        (dir, parent_ws, child_ws, fork)
    }

    #[test]
    fn validate_workspace_name_error_states_the_workspace_and_allowed_set() {
        assert_eq!(
            validate_name("-leading").unwrap_err().to_string(),
            "workspace name must match [A-Za-z0-9_][A-Za-z0-9_-]*: -leading"
        );
    }

    #[test]
    fn workspace_get_accepts_valid_names_and_rejects_invalid_paths() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::with_dirs(
            "alpha".to_string(),
            dir.path().join("alpha"),
            dir.path().join("alpha/config"),
            dir.path().join("state"),
        );
        for name in ["main", "my-workspace_2"] {
            std::fs::create_dir_all(project.worktrees_path().join(name)).unwrap();
            assert_eq!(Workspace::get(&project, name).unwrap().name(), name);
        }
        for name in [".", "..", "main/..", "../../alpha/worktrees/main"] {
            assert!(Workspace::get(&project, name).is_none());
        }
    }

    fn meta_with_edge(fork: Sha) -> ProjectMeta {
        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("parent".to_string(), WorkspaceMeta::default());
        meta.workspaces.insert(
            "child".to_string(),
            WorkspaceMeta {
                parent: Some(ParentEdge {
                    name: "parent".to_string(),
                    base: fork,
                }),
                branch: None,
                integration: None,
            },
        );
        meta
    }

    #[test]
    fn parent_status_is_current_when_parent_advances() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // The parent adds a commit past the branch point of the child.
        commit(&parent_ws.path(), "a\nb\nc\n", "advance");
        let meta = meta_with_edge(fork);
        assert_eq!(
            child_ws.parent_status(&meta).unwrap(),
            ParentStatus::Current
        );
    }

    #[test]
    fn parent_status_is_current_when_anchor_equals_parent_tip() {
        let (_dir, _parent_ws, child_ws, fork) = parent_child();
        // A new fork: the anchor is the parent tip, with no advance and no rewrite.
        let meta = meta_with_edge(fork);
        assert_eq!(
            child_ws.parent_status(&meta).unwrap(),
            ParentStatus::Current
        );
    }

    #[test]
    fn parent_status_is_rewritten_when_parent_amends_below_branch_point() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // The parent amends the commit that the child forked from, which orphans the anchor.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let meta = meta_with_edge(fork);
        match child_ws.parent_status(&meta).unwrap() {
            ParentStatus::Rewritten { dropped } => assert_eq!(dropped, vec![fork]),
            other => panic!("expected Rewritten, got {other:?}"),
        }
    }

    #[test]
    fn branch_base_at_parent_tip_yields_the_tip() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // The child forked at the current tip of the parent.
        let child_head = rev(&child_ws.path(), "HEAD");
        assert_eq!(parent_ws.branch_base(&child_head).unwrap(), fork);
    }

    #[test]
    fn branch_base_below_tip_yields_the_merge_base() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let child_head = rev(&child_ws.path(), "HEAD");
        // The parent advances past the fork point of the child.
        commit(&parent_ws.path(), "a\nb\nc\n", "advance");
        assert_eq!(parent_ws.branch_base(&child_head).unwrap(), fork);
    }

    #[test]
    fn branch_base_errors_on_disjoint_history() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let path = parent_ws.path();
        // A root commit on an orphan branch shares no history with the parent.
        git(&path, &["checkout", "-q", "--orphan", "island"]);
        commit(&path, "island\n", "island root");
        let island = rev(&path, "HEAD");
        git(&path, &["checkout", "-q", &fork.sha()]);

        match parent_ws.branch_base(&island) {
            Err(Error::NoCommonAncestor { .. }) => {}
            other => panic!("expected NoCommonAncestor, got {other:?}"),
        }
    }

    /// Adds a worktree named `name` at the HEAD of `from`, on a branch of the same name.
    fn add_worktree(from: &Path, worktrees: &Path, name: &str) -> PathBuf {
        let path = worktrees.join(name);
        git(
            from,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                name,
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        path
    }

    /// Builds a `WorkspaceMeta` with `parent` as its parent edge.
    fn workspace_meta(parent: Option<(&str, Sha)>) -> WorkspaceMeta {
        WorkspaceMeta {
            parent: parent.map(|(name, base)| ParentEdge {
                name: name.to_string(),
                base,
            }),
            branch: None,
            integration: None,
        }
    }

    #[test]
    fn ancestors_errors_on_dangling_parent() {
        let (_dir, _parent_ws, child_ws, fork) = parent_child();
        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("child".to_string(), workspace_meta(Some(("ghost", fork))));
        match child_ws.ancestors(&meta).next() {
            Some(Err(Error::DanglingParent { workspace, parent })) => {
                assert_eq!(workspace, "child");
                assert_eq!(parent, "ghost");
            }
            other => panic!("expected DanglingParent, got {other:?}"),
        }
    }

    #[test]
    fn plan_reparents_keeps_removed_base_when_child_anchored_past_it() {
        let (_dir, _parent_ws, child_ws, fork) = parent_child();
        let child_tip = rev(&child_ws.path(), "HEAD");
        add_worktree(
            &child_ws.path(),
            &child_ws.project().worktrees_path(),
            "grandchild",
        );
        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("parent".to_string(), workspace_meta(None));
        meta.workspaces
            .insert("child".to_string(), workspace_meta(Some(("parent", fork))));
        meta.workspaces.insert(
            "grandchild".to_string(),
            workspace_meta(Some(("child", child_tip))),
        );

        let plan = plan_reparents(std::slice::from_ref(&child_ws), &meta).unwrap();

        assert_eq!(
            plan.reparents,
            vec![Reparent {
                child: "grandchild".to_string(),
                edge: Some(ParentEdge {
                    name: "parent".to_string(),
                    base: fork,
                }),
            }]
        );
    }

    #[test]
    fn plan_reparents_keeps_child_base_when_anchored_below_removed_base() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let initial = rev(&parent_ws.path(), "HEAD~1");
        add_worktree(
            &child_ws.path(),
            &child_ws.project().worktrees_path(),
            "grandchild",
        );
        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("parent".to_string(), workspace_meta(None));
        meta.workspaces
            .insert("child".to_string(), workspace_meta(Some(("parent", fork))));
        meta.workspaces.insert(
            "grandchild".to_string(),
            workspace_meta(Some(("child", initial))),
        );

        let plan = plan_reparents(std::slice::from_ref(&child_ws), &meta).unwrap();

        assert_eq!(
            plan.reparents,
            vec![Reparent {
                child: "grandchild".to_string(),
                edge: Some(ParentEdge {
                    name: "parent".to_string(),
                    base: initial,
                }),
            }]
        );
    }

    #[test]
    fn plan_reparents_keeps_child_base_after_removed_parent_rebases() {
        let (_dir, parent_ws, child_ws, _fork) = parent_child();
        let child_tip = rev(&child_ws.path(), "HEAD");
        add_worktree(
            &child_ws.path(),
            &child_ws.project().worktrees_path(),
            "grandchild",
        );
        // The parent rewrites its tip, and the edge of the child follows it, so the grandchild
        // stays anchored on the old line.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let rewritten_tip = rev(&parent_ws.path(), "HEAD");
        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("parent".to_string(), workspace_meta(None));
        meta.workspaces.insert(
            "child".to_string(),
            workspace_meta(Some(("parent", rewritten_tip))),
        );
        meta.workspaces.insert(
            "grandchild".to_string(),
            workspace_meta(Some(("child", child_tip))),
        );

        let plan = plan_reparents(std::slice::from_ref(&child_ws), &meta).unwrap();

        assert_eq!(
            plan.reparents,
            vec![Reparent {
                child: "grandchild".to_string(),
                edge: Some(ParentEdge {
                    name: "parent".to_string(),
                    base: child_tip,
                }),
            }]
        );
    }

    #[test]
    fn plan_reparents_orphans_children_of_removed_root() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("parent".to_string(), workspace_meta(None));
        meta.workspaces
            .insert("child".to_string(), workspace_meta(Some(("parent", fork))));

        let plan = plan_reparents(std::slice::from_ref(&parent_ws), &meta).unwrap();

        assert_eq!(
            plan.reparents,
            vec![Reparent {
                child: "child".to_string(),
                edge: None,
            }]
        );
    }

    #[test]
    fn plan_reparents_walks_through_consecutive_removed_targets() {
        // The chain is root -> parent -> child -> grandchild. `parent` and `child` go together, and
        // `grandchild` stays. Its new edge must skip both removed targets and name `root`.
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
        commit(&root_path, "a\n", "initial");
        let root_fork = rev(&root_path, "HEAD");

        let parent_path = add_worktree(&root_path, &worktrees, "parent");
        commit(&parent_path, "a\nparent\n", "parent work");
        let parent_fork = rev(&parent_path, "HEAD");

        let child_path = add_worktree(&parent_path, &worktrees, "child");
        commit(&child_path, "a\nparent\nchild\n", "child work");
        let child_fork = rev(&child_path, "HEAD");

        add_worktree(&child_path, &worktrees, "grandchild");

        let parent_ws = Workspace::new(project.clone(), "parent".to_string());
        let child_ws = Workspace::new(project.clone(), "child".to_string());

        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("root".to_string(), workspace_meta(None));
        meta.workspaces.insert(
            "parent".to_string(),
            workspace_meta(Some(("root", root_fork))),
        );
        meta.workspaces.insert(
            "child".to_string(),
            workspace_meta(Some(("parent", parent_fork))),
        );
        meta.workspaces.insert(
            "grandchild".to_string(),
            workspace_meta(Some(("child", child_fork))),
        );

        let plan = plan_reparents(&[parent_ws, child_ws], &meta).unwrap();

        assert_eq!(
            plan.reparents,
            vec![Reparent {
                child: "grandchild".to_string(),
                edge: Some(ParentEdge {
                    name: "root".to_string(),
                    base: root_fork,
                }),
            }]
        );
    }

    #[test]
    fn plan_repair_prunes_dead_entry_and_reparents_child() {
        // On disk there is `parent` -> `child`, and a `ghost` metadata entry whose worktree is
        // gone. The repair drops `ghost` and leaves the live pair.
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.workspaces
            .insert("ghost".to_string(), workspace_meta(None));

        let plan = plan_repair(&project, &meta).unwrap();
        assert_eq!(plan.pruned(), ["ghost".to_string()]);
        assert_eq!(plan.reparented().count(), 0);
        assert_eq!(plan.stale_default(), None);

        plan.apply(&mut meta);
        assert!(!meta.workspaces.contains_key("ghost"));
        assert!(meta.workspaces.contains_key("parent"));
        assert!(meta.workspaces.contains_key("child"));
    }

    #[test]
    fn plan_repair_reparents_children_of_pruned_parent_onto_root() {
        // On disk there is `root` with a `child`, but `gone`, the parent that the record names for
        // `child`, has no worktree. The repair drops `gone` and makes `child` a root.
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
        commit(&root_path, "a\n", "initial");
        let fork = rev(&root_path, "HEAD");
        add_worktree(&root_path, &worktrees, "child");

        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("root".to_string(), workspace_meta(None));
        meta.workspaces
            .insert("gone".to_string(), workspace_meta(None));
        meta.workspaces
            .insert("child".to_string(), workspace_meta(Some(("gone", fork))));

        let plan = plan_repair(&project, &meta).unwrap();
        assert_eq!(plan.pruned(), ["gone".to_string()]);
        assert_eq!(plan.reparented().collect::<Vec<_>>(), [("child", None)]);

        plan.apply(&mut meta);
        assert!(!meta.workspaces.contains_key("gone"));
        assert_eq!(meta.workspaces["child"].parent, None);
    }

    #[test]
    fn plan_repair_clears_stale_default() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.default = Some("vanished".to_string());

        let plan = plan_repair(&project, &meta).unwrap();
        assert_eq!(plan.stale_default(), Some("vanished"));
        assert!(plan.pruned().is_empty());

        plan.apply(&mut meta);
        assert_eq!(meta.default, None);
    }

    #[test]
    fn plan_repair_is_empty_when_metadata_matches_disk() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let meta = meta_with_edge(fork);
        assert!(plan_repair(&project, &meta).unwrap().is_empty());
    }

    /// Deletes the administrative entry of the `child` worktree. `git worktree prune` leaves that
    /// state after a project move by hand.
    fn delete_child_link(parent_ws: &Workspace) {
        let admin = parent_ws
            .path()
            .join(".git")
            .join("worktrees")
            .join("child");
        std::fs::remove_dir_all(admin).unwrap();
    }

    #[test]
    fn plan_relinks_is_empty_when_links_are_healthy() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let meta = meta_with_edge(fork);
        let anchor = parent_ws.worktree().unwrap();
        assert!(plan_relinks(&project, &meta, &anchor).unwrap().is_empty());
    }

    #[test]
    fn plan_relinks_rebuilds_a_deleted_entry_from_the_recorded_branch() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some("child".to_string());
        delete_child_link(&parent_ws);
        let anchor = parent_ws.worktree().unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        assert_eq!(
            plan.relinks(),
            [Relink {
                workspace: "child".to_string(),
                branch: "child".to_string()
            }]
        );
        assert!(plan.unrepairable().is_empty());
    }

    #[test]
    fn plan_relinks_leaves_a_stale_but_present_entry_to_git_repair() {
        // The administrative entry stays. Only the `.git` file of the worktree points at a location
        // that is gone. `git worktree repair` corrects that without a rebuild, so the plan must
        // stay empty.
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some("child".to_string());
        std::fs::write(
            child_ws.path().join(".git"),
            "gitdir: /nonexistent/worktrees/child\n",
        )
        .unwrap();
        assert!(child_ws.worktree().is_err());
        let anchor = parent_ws.worktree().unwrap();

        assert!(plan_relinks(&project, &meta, &anchor).unwrap().is_empty());
    }

    #[test]
    fn plan_relinks_reports_a_directory_that_never_was_a_worktree() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let meta = meta_with_edge(fork);
        std::fs::create_dir(project.worktrees_path().join("ghost")).unwrap();
        let anchor = parent_ws.worktree().unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        assert!(plan.relinks().is_empty());
        assert_eq!(
            plan.unrepairable(),
            [("ghost".to_string(), Unrepairable::NotAWorktree)]
        );
    }

    #[test]
    fn plan_relinks_reports_a_workspace_without_a_branch_as_unrepairable() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let meta = meta_with_edge(fork);
        delete_child_link(&parent_ws);
        let anchor = parent_ws.worktree().unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        assert!(plan.relinks().is_empty());
        assert_eq!(
            plan.unrepairable(),
            [("child".to_string(), Unrepairable::NoBranch)]
        );
    }

    #[test]
    fn plan_relinks_reports_a_missing_branch_as_unrepairable() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some("vanished".to_string());
        delete_child_link(&parent_ws);
        let anchor = parent_ws.worktree().unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        assert!(plan.relinks().is_empty());
        assert_eq!(
            plan.unrepairable(),
            [(
                "child".to_string(),
                Unrepairable::MissingBranch("vanished".to_string())
            )]
        );
    }

    #[test]
    fn plan_relinks_refuses_a_branch_checked_out_elsewhere() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let parent_branch = parent_ws.worktree().unwrap().get_branch().unwrap();
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some(parent_branch.clone());
        delete_child_link(&parent_ws);
        let anchor = parent_ws.worktree().unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        assert!(plan.relinks().is_empty());
        assert_eq!(
            plan.unrepairable(),
            [(
                "child".to_string(),
                Unrepairable::BranchInUse {
                    branch: parent_branch,
                    holder: "parent".to_string()
                }
            )]
        );
    }

    #[test]
    fn plan_relinks_rebuilds_one_of_two_workspaces_recording_a_branch() {
        let (_dir, parent_ws, _child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let twin_path = project.worktrees_path().join("twin");
        git(
            &parent_ws.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "twin",
                twin_path.to_str().unwrap(),
                "HEAD",
            ],
        );
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some("child".to_string());
        meta.workspaces
            .insert("twin".to_string(), WorkspaceMeta::default());
        meta.workspaces.get_mut("twin").unwrap().branch = Some("child".to_string());
        delete_child_link(&parent_ws);
        let twin_admin = parent_ws.path().join(".git").join("worktrees").join("twin");
        std::fs::remove_dir_all(twin_admin).unwrap();
        let anchor = parent_ws.worktree().unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        assert_eq!(
            plan.relinks(),
            [Relink {
                workspace: "child".to_string(),
                branch: "child".to_string()
            }]
        );
        assert_eq!(
            plan.unrepairable(),
            [(
                "twin".to_string(),
                Unrepairable::BranchInUse {
                    branch: "child".to_string(),
                    holder: "child".to_string()
                }
            )]
        );
    }

    #[test]
    fn relink_worktree_restores_the_link_and_keeps_uncommitted_work() {
        let (_dir, parent_ws, child_ws, _fork) = parent_child();
        let project = parent_ws.project().clone();
        std::fs::write(child_ws.path().join("file.txt"), "uncommitted\n").unwrap();
        delete_child_link(&parent_ws);
        assert!(child_ws.worktree().is_err());

        let anchor = parent_ws.worktree().unwrap();
        let lock = project.lock("test").unwrap();
        anchor
            .relink_worktree(&lock, &child_ws.path(), "child")
            .unwrap();

        let worktree = child_ws.worktree().unwrap();
        assert_eq!(worktree.get_branch().as_deref(), Some("child"));
        let status = worktree.status().unwrap();
        assert!(status.index.is_empty());
        assert_eq!(
            status.worktree,
            [(PathBuf::from("file.txt"), GitFileStatus::Modified)]
        );
    }

    #[test]
    fn relink_worktree_rebuilds_the_index_from_head_dropping_staged_state() {
        // The index went with the pruned entry, so a change staged before the loss comes back
        // unstaged. The edit stays, the staging does not.
        let (_dir, parent_ws, child_ws, _fork) = parent_child();
        let project = parent_ws.project().clone();
        std::fs::write(child_ws.path().join("file.txt"), "staged\n").unwrap();
        git(&child_ws.path(), &["add", "file.txt"]);
        delete_child_link(&parent_ws);

        let anchor = parent_ws.worktree().unwrap();
        let lock = project.lock("test").unwrap();
        anchor
            .relink_worktree(&lock, &child_ws.path(), "child")
            .unwrap();

        let status = child_ws.worktree().unwrap().status().unwrap();
        assert!(status.index.is_empty());
        assert_eq!(
            status.worktree,
            [(PathBuf::from("file.txt"), GitFileStatus::Modified)]
        );
    }

    #[test]
    fn apply_relinks_restores_a_pruned_project_end_to_end() {
        // This is the state after a project move by hand that lost its entries. The entry of the
        // child is gone, and an edit sits uncommitted. One pass must bring the worktree back with
        // the edit unstaged.
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some("child".to_string());
        std::fs::write(child_ws.path().join("file.txt"), "uncommitted\n").unwrap();
        delete_child_link(&parent_ws);
        let anchor = parent_ws.worktree().unwrap();
        let lock = project.lock("test").unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        let applied = apply_relinks(&project, plan, &anchor, &lock).unwrap();

        assert_eq!(applied.relinks().len(), 1);
        assert!(applied.unrepairable().is_empty());
        for ws in project.workspaces().unwrap() {
            assert!(ws.worktree().is_ok());
        }
        let status = child_ws.worktree().unwrap().status().unwrap();
        assert_eq!(
            status.worktree,
            [(PathBuf::from("file.txt"), GitFileStatus::Modified)]
        );
    }

    #[test]
    fn apply_relinks_repairs_a_stale_link_without_rebuilding_it() {
        // This is a plain move: the entry stays, and only the `.git` file is stale. The pass must
        // send it through `git worktree repair`, keep the entry and its index, and report no
        // rebuild.
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let project = parent_ws.project().clone();
        let mut meta = meta_with_edge(fork);
        meta.workspaces.get_mut("child").unwrap().branch = Some("child".to_string());
        std::fs::write(child_ws.path().join("file.txt"), "staged\n").unwrap();
        git(&child_ws.path(), &["add", "file.txt"]);
        std::fs::write(
            child_ws.path().join(".git"),
            "gitdir: /nonexistent/worktrees/child\n",
        )
        .unwrap();
        assert!(child_ws.worktree().is_err());
        let anchor = parent_ws.worktree().unwrap();
        let lock = project.lock("test").unwrap();

        let plan = plan_relinks(&project, &meta, &anchor).unwrap();
        let applied = apply_relinks(&project, plan, &anchor, &lock).unwrap();

        assert!(applied.is_empty());
        let status = child_ws.worktree().unwrap().status().unwrap();
        assert_eq!(
            status.index,
            [(PathBuf::from("file.txt"), GitFileStatus::Modified)]
        );
    }

    #[test]
    fn require_chain_current_passes_on_current_chain() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        commit(&parent_ws.path(), "a\nb\nc\n", "advance");
        let meta = meta_with_edge(fork);
        child_ws.require_chain_current(&parent_ws, &meta).unwrap();
    }

    #[test]
    fn require_chain_current_errors_on_rewritten_edge() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let meta = meta_with_edge(fork);
        match child_ws.require_chain_current(&parent_ws, &meta) {
            Err(Error::ParentRewritten { workspace, dropped }) => {
                assert_eq!(workspace, "child");
                assert_eq!(dropped, vec![fork]);
            }
            other => panic!("expected ParentRewritten, got {other:?}"),
        }
    }

    /// Extends [`parent_child`] with a `grandchild` that forks from the tip of the child, then
    /// amends the parent below the anchor of the child. So the edge of the child is rewritten, and
    /// the edge of the grandchild stays current.
    fn rewritten_ancestor_chain() -> (
        tempfile::TempDir,
        Workspace,
        Workspace,
        Workspace,
        ProjectMeta,
        Sha,
    ) {
        let (dir, parent_ws, child_ws, fork) = parent_child();
        let child_tip = rev(&child_ws.path(), "HEAD");
        add_worktree(
            &child_ws.path(),
            &child_ws.project().worktrees_path(),
            "grandchild",
        );
        let grandchild_ws = Workspace::new(child_ws.project().clone(), "grandchild".to_string());
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let mut meta = meta_with_edge(fork);
        meta.workspaces.insert(
            "grandchild".to_string(),
            workspace_meta(Some(("child", child_tip))),
        );
        (dir, parent_ws, child_ws, grandchild_ws, meta, fork)
    }

    #[test]
    fn first_rewritten_edge_bounded_at_parent_ignores_rewritten_ancestor() {
        let (_dir, _parent_ws, child_ws, grandchild_ws, meta, _fork) = rewritten_ancestor_chain();
        assert!(
            grandchild_ws
                .first_rewritten_edge(&meta, Some(&child_ws))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn first_rewritten_edge_unbounded_finds_rewritten_ancestor() {
        let (_dir, _parent_ws, child_ws, grandchild_ws, meta, fork) = rewritten_ancestor_chain();
        match grandchild_ws.first_rewritten_edge(&meta, None).unwrap() {
            Some((node, dropped)) => {
                assert_eq!(node, child_ws);
                assert_eq!(dropped, vec![fork]);
            }
            None => panic!("expected the child's rewritten edge"),
        }
    }

    #[test]
    fn require_chain_current_stops_at_given_ancestor() {
        let (_dir, _parent_ws, child_ws, grandchild_ws, meta, _fork) = rewritten_ancestor_chain();
        grandchild_ws
            .require_chain_current(&child_ws, &meta)
            .unwrap();
    }

    #[test]
    fn require_chain_current_errors_on_rewritten_edge_between() {
        let (_dir, parent_ws, _child_ws, grandchild_ws, meta, fork) = rewritten_ancestor_chain();
        match grandchild_ws.require_chain_current(&parent_ws, &meta) {
            Err(Error::ParentRewritten { workspace, dropped }) => {
                assert_eq!(workspace, "child");
                assert_eq!(dropped, vec![fork]);
            }
            other => panic!("expected ParentRewritten, got {other:?}"),
        }
    }

    /// Creates a stale anchor through a manual rebase onto an amended parent.
    fn stale_rebase_anchor() -> (tempfile::TempDir, Workspace, Workspace, Sha) {
        let (dir, parent_ws, child_ws, fork) = parent_child();
        // The parent amends the fork commit, and a person rebases the child onto the new tip by
        // hand, but nothing updates the stored anchor. So `fork` is no longer on the line of the
        // child.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let rewritten_fork = rev(&parent_ws.path(), "HEAD");
        git(
            &child_ws.path(),
            &["rebase", "--onto", &rewritten_fork.sha(), &fork.sha()],
        );
        (dir, parent_ws, child_ws, fork)
    }

    #[test]
    fn plan_rebase_rejects_stale_anchor_when_replay_is_needed() {
        let (_dir, parent_ws, child_ws, fork) = stale_rebase_anchor();
        commit(&parent_ws.path(), "a\nb\nparent\n", "advance");
        let meta = meta_with_edge(fork);
        let wt = child_ws.worktree().unwrap();
        let head = wt.head().unwrap();
        let onto_head = parent_ws.worktree().unwrap().head().unwrap();

        for interactive in [false, true] {
            let result = child_ws.plan_rebase(
                &wt,
                &meta,
                &parent_ws,
                &onto_head,
                &head,
                RebaseMode {
                    interactive,
                    all: false,
                },
            );
            match result {
                Err(Error::StaleRebaseAnchor { workspace, anchor }) => {
                    assert_eq!(workspace, child_ws.name());
                    assert_eq!(anchor, fork);
                }
                other => panic!("expected StaleRebaseAnchor, got {other:?}"),
            }
        }
        assert_eq!(wt.head().unwrap().commit(), head.commit());
        assert!(wt.operation_in_progress().is_none());
    }

    #[test]
    fn plan_rebase_skips_replay_with_stale_anchor_when_target_is_ancestor() {
        let (_dir, parent_ws, child_ws, fork) = stale_rebase_anchor();
        let meta = meta_with_edge(fork);
        let wt = child_ws.worktree().unwrap();
        let head = wt.head().unwrap();
        let onto_head = parent_ws.worktree().unwrap().head().unwrap();

        let plan = child_ws
            .plan_rebase(
                &wt,
                &meta,
                &parent_ws,
                &onto_head,
                &head,
                RebaseMode::default(),
            )
            .unwrap();
        assert_eq!(plan, RebasePlan::SkipReplay);
        assert!(matches!(
            child_ws.plan_rebase(
                &wt,
                &meta,
                &parent_ws,
                &onto_head,
                &head,
                RebaseMode {
                    interactive: true,
                    all: false,
                },
            ),
            Err(Error::StaleRebaseAnchor { .. })
        ));
    }

    #[test]
    fn replay_branch_point_all_ignores_stale_anchor() {
        let (_dir, _parent_ws, child_ws, fork) = stale_rebase_anchor();
        let meta = meta_with_edge(fork);
        let wt = child_ws.worktree().unwrap();
        let head = wt.head().unwrap();

        assert_eq!(
            child_ws
                .replay_branch_point(&wt, &meta, &head, true)
                .unwrap(),
            None
        );
    }

    #[test]
    fn info_reports_replay_base_when_anchor_sits_on_the_workspace_line() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        commit(&parent_ws.path(), "a\nb\nc\n", "advance");
        let meta = meta_with_edge(fork.clone());

        let info = child_ws.info(&meta).unwrap();

        assert_eq!(info.parent.unwrap().replay_base, Some(fork));
    }

    #[test]
    fn info_reports_no_replay_base_when_anchor_drifted_off_the_workspace_line() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // This is the drift of `stale_rebase_anchor`. A
        // person rebases the child onto an amended parent tip by hand, and the stored anchor stays
        // behind, so a replay from the stored anchor fails.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let rewritten_fork = rev(&parent_ws.path(), "HEAD");
        git(
            &child_ws.path(),
            &["rebase", "--onto", &rewritten_fork.sha(), &fork.sha()],
        );
        let meta = meta_with_edge(fork);

        let info = child_ws.info(&meta).unwrap();

        assert_eq!(info.parent.unwrap().replay_base, None);
    }

    #[test]
    fn nearest_common_ancestor_of_descendant_and_ancestor() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        assert_eq!(
            nearest_common_ancestor(&child_ws, &parent_ws, &meta).unwrap(),
            Some(parent_ws)
        );
    }

    #[test]
    fn nearest_common_ancestor_of_siblings() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "sibling",
        );
        let sibling_ws = Workspace::new(parent_ws.project().clone(), "sibling".to_string());
        let mut meta = meta_with_edge(fork);
        meta.workspaces.insert(
            "sibling".to_string(),
            workspace_meta(Some(("parent", fork))),
        );
        assert_eq!(
            nearest_common_ancestor(&child_ws, &sibling_ws, &meta).unwrap(),
            Some(parent_ws)
        );
    }

    #[test]
    fn nearest_common_ancestor_of_disjoint_trees() {
        let (_dir, parent_ws, _child_ws, _fork) = parent_child();
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "island",
        );
        let island_ws = Workspace::new(parent_ws.project().clone(), "island".to_string());
        let meta = ProjectMeta::default();
        assert_eq!(
            nearest_common_ancestor(&parent_ws, &island_ws, &meta).unwrap(),
            None
        );
    }

    #[test]
    fn require_merge_chains_current_errors_when_dest_chain_rewritten_sibling() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "sibling",
        );
        let sibling_ws = Workspace::new(parent_ws.project().clone(), "sibling".to_string());
        let mut meta = meta_with_edge(fork);
        meta.workspaces.insert(
            "sibling".to_string(),
            workspace_meta(Some(("parent", fork))),
        );
        // The parent amends the commit that both children forked from, which rewrites the edge of
        // the destination side, `child`.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );

        match require_merge_chains_current(&child_ws, &sibling_ws, &meta) {
            Err(Error::ParentRewritten { workspace, .. }) => assert_eq!(workspace, "child"),
            other => panic!("expected ParentRewritten, got {other:?}"),
        }
    }

    #[test]
    fn require_merge_chains_current_errors_when_source_chain_rewritten_cousin() {
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
        commit(&root_path, "a\n", "initial");
        let root_initial = rev(&root_path, "HEAD");
        commit(&root_path, "a\nb\n", "second");
        let root_second = rev(&root_path, "HEAD");

        // `aunt_a` forks from the earlier commit, and `aunt_b` forks from the tip. So an amend of
        // the tip breaks the fork point of `aunt_b` alone.
        let aunt_a_path = worktrees.join("aunt_a");
        git(
            &root_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "aunt_a",
                aunt_a_path.to_str().unwrap(),
                &root_initial.sha(),
            ],
        );
        commit(&aunt_a_path, "a\naunt_a\n", "aunt_a work");
        let cousin1_path = add_worktree(&aunt_a_path, &worktrees, "cousin1");
        commit(&cousin1_path, "a\naunt_a\ncousin1\n", "cousin1 work");
        let cousin1_fork = rev(&aunt_a_path, "HEAD");

        let aunt_b_path = worktrees.join("aunt_b");
        git(
            &root_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "aunt_b",
                aunt_b_path.to_str().unwrap(),
                &root_second.sha(),
            ],
        );
        commit(&aunt_b_path, "a\nb\naunt_b\n", "aunt_b work");
        let cousin2_path = add_worktree(&aunt_b_path, &worktrees, "cousin2");
        commit(&cousin2_path, "a\nb\naunt_b\ncousin2\n", "cousin2 work");
        let cousin2_fork = rev(&aunt_b_path, "HEAD");

        let cousin1_ws = Workspace::new(project.clone(), "cousin1".to_string());
        let cousin2_ws = Workspace::new(project.clone(), "cousin2".to_string());

        let mut meta = ProjectMeta::default();
        meta.workspaces
            .insert("root".to_string(), workspace_meta(None));
        meta.workspaces.insert(
            "aunt_a".to_string(),
            workspace_meta(Some(("root", root_initial))),
        );
        meta.workspaces.insert(
            "cousin1".to_string(),
            workspace_meta(Some(("aunt_a", cousin1_fork))),
        );
        meta.workspaces.insert(
            "aunt_b".to_string(),
            workspace_meta(Some(("root", root_second))),
        );
        meta.workspaces.insert(
            "cousin2".to_string(),
            workspace_meta(Some(("aunt_b", cousin2_fork))),
        );

        // The root amends its tip, which is the fork point of `aunt_b`. That rewrites the edge of
        // `aunt_b` and leaves the edge of `aunt_a`, which forks from the earlier commit, as it was.
        git(&root_path, &["commit", "-q", "--amend", "-m", "rewritten"]);

        match require_merge_chains_current(&cousin1_ws, &cousin2_ws, &meta) {
            Err(Error::ParentRewritten { workspace, .. }) => assert_eq!(workspace, "aunt_b"),
            other => panic!("expected ParentRewritten, got {other:?}"),
        }
    }

    #[test]
    fn require_merge_chains_current_passes_when_both_chains_current() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "sibling",
        );
        let sibling_ws = Workspace::new(parent_ws.project().clone(), "sibling".to_string());
        let mut meta = meta_with_edge(fork);
        meta.workspaces.insert(
            "sibling".to_string(),
            workspace_meta(Some(("parent", fork))),
        );

        require_merge_chains_current(&child_ws, &sibling_ws, &meta).unwrap();
    }

    #[test]
    fn require_merge_chains_current_is_noop_for_disjoint_trees() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "island",
        );
        let island_ws = Workspace::new(parent_ws.project().clone(), "island".to_string());
        let meta = meta_with_edge(fork);

        require_merge_chains_current(&child_ws, &island_ws, &meta).unwrap();
    }

    #[test]
    fn plan_rebase_rejects_a_parent_cycle() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        let wt = child_ws.worktree().unwrap();
        let head = wt.head().unwrap();
        for onto in [&child_ws, &parent_ws] {
            let mut meta = meta.clone();
            if onto == &parent_ws {
                meta.workspaces
                    .entry(parent_ws.name().to_string())
                    .or_default()
                    .parent = Some(ParentEdge {
                    name: child_ws.name().to_string(),
                    base: head.commit(),
                });
            }
            let onto_head = onto.worktree().unwrap().head().unwrap();
            assert!(matches!(
                child_ws.plan_rebase(&wt, &meta, onto, &onto_head, &head, RebaseMode::default()),
                Err(Error::CycleDetected(_))
            ));
        }
    }

    #[test]
    fn plan_rebase_skips_replay_when_base_tip_is_ancestor_of_head() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        let child_wt = child_ws.worktree().unwrap();
        let base_head = parent_ws.worktree().unwrap().head().unwrap();
        let head = child_wt.head().unwrap();

        let plan = child_ws
            .plan_rebase(
                &child_wt,
                &meta,
                &parent_ws,
                &base_head,
                &head,
                RebaseMode::default(),
            )
            .unwrap();

        assert_eq!(plan, RebasePlan::SkipReplay);
    }

    #[test]
    fn plan_rebase_replays_own_commits_onto_an_ancestor_below_the_fork() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        let root_path = parent_ws.project().worktrees_path().join("root");
        git(
            &parent_ws.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "root",
                root_path.to_str().unwrap(),
                "HEAD^",
            ],
        );
        let root_ws = Workspace::new(parent_ws.project().clone(), "root".to_string());
        let child_wt = child_ws.worktree().unwrap();
        let head = child_wt.head().unwrap();
        let root_head = root_ws.worktree().unwrap().head().unwrap();

        let plan = child_ws
            .plan_rebase(
                &child_wt,
                &meta,
                &root_ws,
                &root_head,
                &head,
                RebaseMode::default(),
            )
            .unwrap();

        assert_eq!(
            plan,
            RebasePlan::Replay {
                branch_point: Some(fork)
            }
        );
    }

    #[test]
    fn plan_rebase_replays_own_commits_when_interactive_and_base_unmoved() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        let child_wt = child_ws.worktree().unwrap();
        let base_head = parent_ws.worktree().unwrap().head().unwrap();
        let head = child_wt.head().unwrap();

        // The base tip is an ancestor of HEAD, so a plain rebase skips the replay. An interactive
        // rebase must still replay the commits of the child from the fork, so the todo editor
        // opens.
        let plan = child_ws
            .plan_rebase(
                &child_wt,
                &meta,
                &parent_ws,
                &base_head,
                &head,
                RebaseMode {
                    interactive: true,
                    all: false,
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            RebasePlan::Replay {
                branch_point: Some(fork)
            }
        );
    }

    #[test]
    fn plan_rebase_replays_from_stored_anchor_when_parent_rewrote() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let child_wt = child_ws.worktree().unwrap();
        let base_head = parent_ws.worktree().unwrap().head().unwrap();
        let head = child_wt.head().unwrap();

        let plan = child_ws
            .plan_rebase(
                &child_wt,
                &meta,
                &parent_ws,
                &base_head,
                &head,
                RebaseMode::default(),
            )
            .unwrap();

        assert_eq!(
            plan,
            RebasePlan::Replay {
                branch_point: Some(fork)
            }
        );
    }

    #[test]
    fn plan_rebase_all_guard_errors_on_rewritten_stored_parent() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let child_wt = child_ws.worktree().unwrap();
        let base_head = parent_ws.worktree().unwrap().head().unwrap();
        let head = child_wt.head().unwrap();

        let mode = RebaseMode {
            interactive: false,
            all: true,
        };
        match child_ws.plan_rebase(&child_wt, &meta, &parent_ws, &base_head, &head, mode) {
            Err(Error::ParentRewritten { workspace, .. }) => assert_eq!(workspace, "child"),
            other => panic!("expected ParentRewritten, got {other:?}"),
        }
    }

    #[test]
    fn plan_rebase_all_guard_passes_on_current_stored_parent() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        let meta = meta_with_edge(fork);
        commit(&parent_ws.path(), "a\nb\nc\n", "advance");
        let child_wt = child_ws.worktree().unwrap();
        let base_head = parent_ws.worktree().unwrap().head().unwrap();
        let head = child_wt.head().unwrap();

        let plan = child_ws
            .plan_rebase(
                &child_wt,
                &meta,
                &parent_ws,
                &base_head,
                &head,
                RebaseMode {
                    interactive: false,
                    all: true,
                },
            )
            .unwrap();

        // `--all` ignores the stored anchor and replays the whole branch.
        assert_eq!(plan, RebasePlan::Replay { branch_point: None });
    }

    #[test]
    /// The captured cherry-pick, squash, and commit variants capture stdout and report success,
    /// same as their inherited counterparts.
    fn worktree_captured_variants_report_success_and_capture_stdout() {
        let (_dir, parent_ws, child_ws, _fork) = parent_child();
        // A commit on a file the child never touches, so the cherry-pick below applies without
        // conflict against the child's own `file.txt` commit.
        std::fs::write(parent_ws.path().join("other.txt"), "one\n").unwrap();
        git(&parent_ws.path(), &["add", "other.txt"]);
        git(&parent_ws.path(), &["commit", "-q", "-m", "parent only"]);
        let parent_head = parent_ws.worktree().unwrap().head().unwrap();
        let project = child_ws.project().clone();
        let lock = project.lock("test").unwrap();
        let child_wt = child_ws.worktree().unwrap();

        let picked = child_wt
            .cherry_pick_commits_captured(&lock, std::slice::from_ref(&parent_head))
            .unwrap();
        assert!(picked.status.success());
    }

    #[test]
    /// The captured squash and commit variants capture stdout and report success, same as their
    /// inherited counterparts.
    fn worktree_captured_squash_and_commit_report_success() {
        let (_dir, parent_ws, child_ws, _fork) = parent_child();
        // A commit on a file the child never touches, so the squash merge below applies without
        // conflict against the child's own `file.txt` commit.
        std::fs::write(parent_ws.path().join("other.txt"), "one\n").unwrap();
        git(&parent_ws.path(), &["add", "other.txt"]);
        git(&parent_ws.path(), &["commit", "-q", "-m", "parent only"]);
        let parent_head = parent_ws.worktree().unwrap().head().unwrap();
        let project = child_ws.project().clone();
        let lock = project.lock("test").unwrap();
        let child_wt = child_ws.worktree().unwrap();

        let squashed = child_wt.squash_merge_captured(&lock, &parent_head).unwrap();
        assert!(squashed.status.success());

        let committed = child_wt
            .create_commit_captured(&lock, "squash commit")
            .unwrap();
        assert!(committed.status.success());
    }

    #[test]
    fn rebase_onto_reanchors_only_after_a_successful_replay() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // An amend of the message alone leaves the tree content as it was, so the replay has no
        // conflict.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let project = child_ws.project().clone();
        let lock = project.lock("test").unwrap();

        let base_head = parent_ws.worktree().unwrap().head().unwrap();
        let plan = RebasePlan::Replay {
            branch_point: Some(fork),
        };

        let outcome = child_ws
            .rebase_onto(
                &lock,
                &parent_ws,
                &base_head,
                &plan,
                RebaseIo {
                    interactive: false,
                    quiet: true,
                    on_output: None,
                },
            )
            .unwrap();

        assert_eq!(outcome, RebaseOutcome::Reanchored);
        // The replay must leave the workspace on its branch, not on a detached HEAD, which a `HEAD`
        // in the `<branch>` positional of the rebase caused.
        assert_eq!(
            child_ws.worktree().unwrap().get_branch().as_deref(),
            Some("child")
        );
        let updated = project.metadata().read().unwrap();
        let edge = child_ws.parent_edge(&updated).unwrap();
        assert_eq!(edge.name, "parent");
        assert_eq!(edge.base, base_head.commit());
    }

    #[test]
    fn rebase_onto_leaves_stored_anchor_untouched_when_replay_fails() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // The advance of the parent conflicts with the commit of the child. Each one adds a
        // different third line to the same file.
        commit(&parent_ws.path(), "a\nb\nPARENT\n", "advance");
        let project = child_ws.project().clone();
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |m| {
                m.workspaces
                    .insert("child".to_string(), workspace_meta(Some(("parent", fork))));
                Ok(())
            })
            .unwrap();

        let plan = RebasePlan::Replay {
            branch_point: Some(fork),
        };

        let onto_head = parent_ws.worktree().unwrap().head().unwrap();
        let result = child_ws.rebase_onto(
            &lock,
            &parent_ws,
            &onto_head,
            &plan,
            RebaseIo {
                interactive: false,
                quiet: true,
                on_output: None,
            },
        );

        assert!(result.is_err());
        git(&child_ws.path(), &["rebase", "--abort"]);

        let updated = project.metadata().read().unwrap();
        let edge = child_ws.parent_edge(&updated).unwrap();
        assert_eq!(edge.base, fork);
    }

    #[test]
    /// In JSON mode a conflicting replay still forwards its `Auto-merging` and `CONFLICT` lines,
    /// captured from stdout, before the rebase reports its failure.
    fn rebase_onto_forwards_captured_output_on_a_conflicting_replay() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        commit(&parent_ws.path(), "a\nb\nPARENT\n", "advance");
        let project = child_ws.project().clone();
        let lock = project.lock("test").unwrap();

        let plan = RebasePlan::Replay {
            branch_point: Some(fork),
        };
        let onto_head = parent_ws.worktree().unwrap().head().unwrap();

        let mut lines = Vec::new();
        let mut sink = |line: &str| lines.push(line.to_string());
        let result = child_ws.rebase_onto(
            &lock,
            &parent_ws,
            &onto_head,
            &plan,
            RebaseIo {
                interactive: false,
                quiet: true,
                on_output: Some(&mut sink),
            },
        );

        assert!(result.is_err());
        assert!(
            lines.iter().any(|l| l.contains("CONFLICT")),
            "expected a CONFLICT line, got {lines:?}"
        );
        git(&child_ws.path(), &["rebase", "--abort"]);
    }

    #[test]
    fn rebase_onto_defers_reanchor_when_interactive_rebase_stops_mid_flight() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // An amend of the message alone leaves the tree content as it was, so the replay has no
        // conflict. What counts here is that it pauses, not that it fails.
        git(
            &parent_ws.path(),
            &["commit", "-q", "--amend", "-m", "rewritten"],
        );
        let project = child_ws.project().clone();
        let lock = project.lock("test").unwrap();
        project
            .metadata()
            .update(&lock, |m| {
                m.workspaces
                    .insert("child".to_string(), workspace_meta(Some(("parent", fork))));
                Ok(())
            })
            .unwrap();

        // A sequence editor that puts `break` on the first todo line. So the interactive rebase
        // stops in the middle, before it replays a commit, and exits 0 with the rebase still in
        // progress.
        let editor_dir = tempfile::tempdir().unwrap();
        let editor_path = editor_dir.path().join("break-editor.sh");
        std::fs::write(
            &editor_path,
            "#!/bin/sh\n{ printf 'break\\n'; cat \"$1\"; } > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&editor_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&editor_path, perms).unwrap();

        let plan = RebasePlan::Replay {
            branch_point: Some(fork),
        };

        git(
            &child_ws.path(),
            &["config", "sequence.editor", editor_path.to_str().unwrap()],
        );
        let onto_head = parent_ws.worktree().unwrap().head().unwrap();
        let result = child_ws.rebase_onto(
            &lock,
            &parent_ws,
            &onto_head,
            &plan,
            RebaseIo {
                interactive: true,
                quiet: true,
                on_output: None,
            },
        );
        assert_eq!(result.unwrap(), RebaseOutcome::StoppedMidRebase);

        let worktree = child_ws.worktree().unwrap();
        assert!(matches!(
            worktree.operation_in_progress(),
            Some(InProgressOperation::Rebase { .. })
        ));

        let updated = project.metadata().read().unwrap();
        let edge = child_ws.parent_edge(&updated).unwrap();
        assert_eq!(edge.base, fork);

        git(&child_ws.path(), &["rebase", "--abort"]);
    }

    #[test]
    fn plan_insert_parent_mid_chain() {
        let (_dir, parent_ws, child_ws, fork) = parent_child();
        // The new workspace forks at the tip of the former parent, which is the start point that
        // `--parent-of` of `cmd_add` uses.
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "new",
        );
        let new_ws = Workspace::new(parent_ws.project().clone(), "new".to_string());
        let former_parent_edge = ParentEdge {
            name: "parent".to_string(),
            base: fork,
        };

        let (new_ws_edge, child_edge) =
            plan_insert_parent(&child_ws, Some(&former_parent_edge), &new_ws).unwrap();

        assert_eq!(
            new_ws_edge,
            Some(ParentEdge {
                name: "parent".to_string(),
                base: fork,
            })
        );
        assert_eq!(
            child_edge,
            ParentEdge {
                name: "new".to_string(),
                base: fork,
            }
        );
    }

    #[test]
    fn plan_insert_parent_above_a_root() {
        let (_dir, parent_ws, _child_ws, _fork) = parent_child();
        // Insert "new" above `parent_ws`, which is a root and has no former parent. This is what
        // `--parent-of` of `cmd_add` does on a root workspace.
        add_worktree(
            &parent_ws.path(),
            &parent_ws.project().worktrees_path(),
            "new",
        );
        let new_ws = Workspace::new(parent_ws.project().clone(), "new".to_string());
        let parent_tip = rev(&parent_ws.path(), "HEAD");

        let (new_ws_edge, child_edge) = plan_insert_parent(&parent_ws, None, &new_ws).unwrap();

        assert_eq!(new_ws_edge, None);
        assert_eq!(
            child_edge,
            ParentEdge {
                name: "new".to_string(),
                base: parent_tip,
            }
        );
    }
}
