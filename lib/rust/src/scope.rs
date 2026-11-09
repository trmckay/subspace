use std::path::Path;

use crate::{
    error::{Error, Result},
    project::{Project, WORKTREES_DIR},
    registry::GlobalState,
    workspace::Workspace,
};

/// Every workspace reference sigil, in its `@`-prefixed form.
pub const SIGILS: [&str; 3] = ["@current", "@parent", "@root"];

/// A workspace reference sigil (`@parent`, `@root`, `@current`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sigil {
    /// The parent workspace (`@parent`).
    Parent,
    /// The root of the workspace tree (`@root`).
    Root,
    /// The current workspace (`@current`).
    Current,
}

impl Sigil {
    /// Parses a sigil string (with or without `@` prefix).
    pub fn parse(s: &str) -> Option<Sigil> {
        let bare = s.strip_prefix('@').unwrap_or(s);
        match bare {
            "parent" => Some(Sigil::Parent),
            "root" => Some(Sigil::Root),
            "current" => Some(Sigil::Current),
            _ => None,
        }
    }

    /// Resolves this sigil relative to `current` workspace.
    pub fn resolve(self, current: &Workspace) -> Result<Workspace> {
        let meta = current.project().metadata().read()?;
        match self {
            Sigil::Parent => current
                .parent_workspace(&meta)
                .ok_or_else(|| Error::WorkspaceNotFound("@parent".into())),
            Sigil::Root => current.root_workspace(&meta),
            Sigil::Current => Ok(current.clone()),
        }
    }
}

/// Resolved scope: either a project or a workspace within a project.
pub enum Scope {
    /// Scoped to a project (no specific workspace).
    Project(Project),
    /// Scoped to a specific workspace within a project.
    Workspace(Workspace),
}

/// Context required to apply a workspace operation.
pub struct Context {
    /// Where the operation applies.
    pub scope: Scope,
    /// Where the command was issued from, for argument parsing. `None` when CWD is outside any
    /// workspace. Private so reads go through [`Context::origin`], which runs the cross-project
    /// check lazily (see [`origin_in_project`]).
    origin: Option<Workspace>,
}

/// Scope constraint for [`Context::resolve_target`].
pub enum TargetScope {
    /// The target must be within the current scope's project.
    SameProject,
    /// The target can be in any project. Governs only a qualified `project/workspace` target: a
    /// `@sigil` target always resolves against the origin, which [`origin_in_project`] confines to
    /// the scoped project.
    Any,
}

/// Returns `origin` when it lies inside `project`, or `Error::CrossProjectOrigin` when it does not:
/// a workspace outside the scoped project can serve neither as a default nor as a sigil base.
///
/// The check runs lazily, at each read, rather than eagerly at [`Context`] construction. `subspace
/// -P <project> -w <workspace> <cmd>` (for example `config/tmux/scripts/tmux-status`) scopes to another
/// project from a pane whose CWD sits in some other project's workspace, and that call never reads
/// the origin. An eager rejection breaks it, and discards the information
/// [`Error::CrossProjectOrigin`] needs to name both projects.
fn origin_in_project<'a>(
    origin: Option<&'a Workspace>,
    project: &Project,
) -> Result<Option<&'a Workspace>> {
    match origin {
        Some(ws) if ws.project() != project => Err(Error::CrossProjectOrigin {
            scope: project.name().to_string(),
            origin: ws.project().name().to_string(),
        }),
        origin => Ok(origin),
    }
}

impl Context {
    /// Builds a context scoped to `ws`, with `ws` as both the scope and the origin.
    pub fn for_workspace(ws: Workspace) -> Context {
        Context {
            scope: Scope::Workspace(ws.clone()),
            origin: Some(ws),
        }
    }

    /// Returns the workspace the command was issued from, or `None` when it was issued outside any
    /// workspace. See [`origin_in_project`] for the cross-project check this applies.
    pub fn origin(&self) -> Result<Option<&Workspace>> {
        origin_in_project(self.origin.as_ref(), self.scope.project())
    }

    /// Returns the origin workspace. Fails when the command was issued outside any workspace or
    /// from a different project than the scope.
    pub fn require_origin(&self) -> Result<&Workspace> {
        self.origin()?.ok_or(Error::NoScope)
    }

    /// Resolves a target string (sigil, `project/workspace`, or bare name) to a [`Workspace`].
    ///
    /// A `@sigil` target resolves against the origin workspace (see [`origin_in_project`]).
    pub fn resolve_target(
        &self,
        registry: &GlobalState,
        target: &str,
        scope: TargetScope,
    ) -> Result<Workspace> {
        if target.starts_with('@') {
            self.require_origin()?.resolve_sigil(target)
        } else if let Some((project_name, ws_name)) = target.split_once('/') {
            match scope {
                TargetScope::Any => {
                    let project = Project::get(registry, project_name)?
                        .ok_or_else(|| Error::ProjectNotFound(project_name.to_string()))?;
                    Workspace::get(&project, ws_name)
                        .ok_or_else(|| Error::WorkspaceNotFound(target.to_string()))
                }
                TargetScope::SameProject if project_name == self.scope.project().name() => {
                    Workspace::get(self.scope.project(), ws_name)
                        .ok_or_else(|| Error::WorkspaceNotFound(ws_name.to_string()))
                }
                TargetScope::SameProject => Err(Error::CrossProjectTarget {
                    expected: self.scope.project().name().to_string(),
                    actual: target.to_string(),
                }),
            }
        } else {
            Workspace::get(self.scope.project(), target)
                .ok_or_else(|| Error::WorkspaceNotFound(target.to_string()))
        }
    }
}

impl Scope {
    /// Returns the project, regardless of scope variant.
    pub fn project(&self) -> &Project {
        match self {
            Scope::Project(p) => p,
            Scope::Workspace(ws) => ws.project(),
        }
    }

    /// Returns the workspace if this is a workspace scope.
    pub fn workspace(&self) -> Option<&Workspace> {
        match self {
            Scope::Workspace(ws) => Some(ws),
            Scope::Project(_) => None,
        }
    }

    /// Returns the workspace, or an error if this is a project scope.
    pub fn require_workspace(&self) -> Result<&Workspace> {
        self.workspace().ok_or(Error::NoScope)
    }

    /// Derives a scope from `path` by matching it against the registered project directories: the
    /// project whose canonical path is the longest prefix of the canonical `path` owns it, and the
    /// remainder (`worktrees/<ws>`) names the workspace.
    pub fn from_path(registry: &GlobalState, path: &Path) -> Result<Option<Scope>> {
        let path = match path.canonicalize() {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // A project can nest inside another's directory, so the longest matching prefix wins.
        let mut best: Option<(String, std::path::PathBuf)> = None;
        for (name, project_path) in registry.all() {
            let Ok(canonical) = project_path.canonicalize() else {
                continue;
            };
            if !path.starts_with(&canonical) {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, current)| canonical.as_os_str().len() > current.as_os_str().len())
            {
                best = Some((name.to_string(), canonical));
            }
        }

        let Some((name, project_path)) = best else {
            return Ok(None);
        };
        let project = match Project::open(name, project_path.clone())? {
            Some(p) => p,
            None => return Ok(None),
        };

        let rel = path.strip_prefix(&project_path).unwrap();
        let mut components = rel.components().filter_map(|c| c.as_os_str().to_str());
        if let (Some(worktrees), Some(ws_name)) = (components.next(), components.next())
            && worktrees == WORKTREES_DIR
        {
            return Ok(Some(match Workspace::get(&project, ws_name) {
                Some(ws) => Scope::Workspace(ws),
                None => Scope::Project(project),
            }));
        }
        Ok(Some(Scope::Project(project)))
    }

    /// Resolves scope from CLI flags (`-P`, `-p`, `-w`) and CWD.
    pub fn resolve(
        registry: &GlobalState,
        project_name: Option<&str>,
        workspace_name: Option<&str>,
        cwd: &Path,
    ) -> Result<Option<Context>> {
        let cwd_scope = Scope::from_path(registry, cwd)?;
        let origin = cwd_scope.as_ref().and_then(|s| match s {
            Scope::Workspace(ws) => Some(ws.clone()),
            Scope::Project(_) => None,
        });

        let project = match project_name {
            Some("") | None => match &cwd_scope {
                Some(s) => s.project().clone(),
                None => return Ok(None),
            },
            Some(name) => match Project::get(registry, name)? {
                Some(p) => p,
                None => return Ok(None),
            },
        };

        let scope = match workspace_name {
            None if project_name.is_some() => Scope::Project(project),
            None => match &origin {
                Some(ws) => Scope::Workspace(ws.clone()),
                None => return Ok(None),
            },
            Some(alias) if alias.starts_with('@') => {
                // A sigil resolves within the origin's own project, so an origin outside `project`
                // can never name a workspace inside it.
                match origin_in_project(origin.as_ref(), &project)? {
                    Some(ws) => Scope::Workspace(ws.resolve_sigil(alias)?),
                    None => return Ok(None),
                }
            }
            Some(name) => {
                let bare = match name.split_once('/') {
                    Some((prefix, rest)) if prefix == project.name() => rest,
                    Some(_) => return Ok(None),
                    None => name,
                };
                match Workspace::get(&project, bare) {
                    Some(ws) => Scope::Workspace(ws),
                    None => return Ok(None),
                }
            }
        };

        Ok(Some(Context { scope, origin }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{commit, git};
    use std::path::PathBuf;

    /// Builds two sibling projects, `alpha` and `beta`, each with a single `main` workspace and one
    /// commit, and registers both. Returns the canonicalized parent directory and the registry,
    /// beside the tempdir that keeps them alive.
    fn two_projects() -> (tempfile::TempDir, PathBuf, GlobalState) {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = GlobalState::default();
        for project in ["alpha", "beta"] {
            let ws = dir.path().join(project).join(WORKTREES_DIR).join("main");
            std::fs::create_dir_all(&ws).unwrap();
            git(&ws, &["init", "-q"]);
            commit(&ws, "x\n", "initial");
        }
        let pdir = dir.path().canonicalize().unwrap();
        for project in ["alpha", "beta"] {
            registry.insert(project, pdir.join(project)).unwrap();
        }
        (dir, pdir, registry)
    }

    fn workspace_path(pdir: &Path, project: &str) -> PathBuf {
        pdir.join(project).join(WORKTREES_DIR).join("main")
    }

    /// Builds a project `outer` at the tempdir root, and a second project `inner` registered at a
    /// path nested inside `outer`'s `main` workspace (`outer/worktrees/main/inner`). Both hold a
    /// single `main` workspace with one commit. Returns the canonicalized tempdir and the registry,
    /// beside the tempdir that keeps them alive.
    fn nested_projects() -> (tempfile::TempDir, PathBuf, GlobalState) {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = GlobalState::default();

        let outer_ws = dir.path().join("outer").join(WORKTREES_DIR).join("main");
        std::fs::create_dir_all(&outer_ws).unwrap();
        git(&outer_ws, &["init", "-q"]);
        commit(&outer_ws, "x\n", "initial");

        let inner_dir = outer_ws.join("inner");
        let inner_ws = inner_dir.join(WORKTREES_DIR).join("main");
        std::fs::create_dir_all(&inner_ws).unwrap();
        git(&inner_ws, &["init", "-q"]);
        commit(&inner_ws, "x\n", "initial");

        let root = dir.path().canonicalize().unwrap();
        registry.insert("outer", root.join("outer")).unwrap();
        registry
            .insert(
                "inner",
                root.join("outer")
                    .join(WORKTREES_DIR)
                    .join("main")
                    .join("inner"),
            )
            .unwrap();

        (dir, root, registry)
    }

    #[test]
    fn from_path_resolves_nested_project_via_longest_prefix() {
        let (_dir, root, registry) = nested_projects();
        let path = root
            .join("outer")
            .join(WORKTREES_DIR)
            .join("main")
            .join("inner")
            .join(WORKTREES_DIR)
            .join("main");
        let scope = Scope::from_path(&registry, &path).unwrap().unwrap();
        let ws = scope.workspace().unwrap();
        assert_eq!(ws.project().name(), "inner");
        assert_eq!(ws.name(), "main");
    }

    #[test]
    fn from_path_falls_back_to_project_scope_at_the_project_root() {
        let (_dir, pdir, registry) = two_projects();
        let scope = Scope::from_path(&registry, &pdir.join("alpha"))
            .unwrap()
            .unwrap();
        assert!(scope.workspace().is_none());
        assert_eq!(scope.project().name(), "alpha");
    }

    #[test]
    fn from_path_falls_back_to_project_scope_in_a_non_worktrees_subdir() {
        let (_dir, pdir, registry) = two_projects();
        let other_dir = pdir.join("alpha").join("some-other-dir");
        std::fs::create_dir_all(&other_dir).unwrap();
        let scope = Scope::from_path(&registry, &other_dir).unwrap().unwrap();
        assert!(scope.workspace().is_none());
        assert_eq!(scope.project().name(), "alpha");
    }

    #[test]
    fn origin_is_rejected_when_scoped_to_another_project() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("beta"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            ctx.origin(),
            Err(Error::CrossProjectOrigin { scope, origin }) if scope == "beta" && origin == "alpha"
        ));
    }

    #[test]
    fn origin_is_available_within_the_same_project() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("alpha"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        let ws = ctx.origin().unwrap().unwrap();
        assert_eq!(ws.name(), "main");
        assert_eq!(ws.project().name(), "alpha");
    }

    #[test]
    fn sigil_target_is_rejected_when_scoped_to_another_project() {
        let (_dir, pdir, registry) = two_projects();
        for sigil in SIGILS {
            let ctx = Scope::resolve(
                &registry,
                Some("beta"),
                None,
                &workspace_path(&pdir, "alpha"),
            )
            .unwrap()
            .unwrap();
            assert!(matches!(
                ctx.resolve_target(&registry, sigil, TargetScope::SameProject),
                Err(Error::CrossProjectOrigin { scope, origin }) if scope == "beta" && origin == "alpha"
            ));
        }
    }

    #[test]
    fn sigil_target_resolves_within_the_same_project() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("alpha"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        let ws = ctx
            .resolve_target(&registry, "@current", TargetScope::SameProject)
            .unwrap();
        assert_eq!(ws.name(), "main");
        assert_eq!(ws.project().name(), "alpha");
    }

    #[test]
    fn workspace_flag_sigil_is_rejected_across_projects() {
        let (_dir, pdir, registry) = two_projects();
        assert!(matches!(
            Scope::resolve(
                &registry,
                Some("beta"),
                Some("@current"),
                &workspace_path(&pdir, "alpha")
            ),
            Err(Error::CrossProjectOrigin { scope, origin }) if scope == "beta" && origin == "alpha"
        ));
    }

    #[test]
    fn workspace_flag_sigil_resolves_within_the_same_project() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("alpha"),
            Some("@current"),
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ctx.scope.require_workspace().unwrap().name(), "main");
    }

    #[test]
    fn qualified_target_still_rejects_a_foreign_project() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("alpha"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            ctx.resolve_target(&registry, "beta/main", TargetScope::SameProject),
            Err(Error::CrossProjectTarget { .. })
        ));
    }

    #[test]
    fn qualified_target_resolves_under_any_from_a_foreign_origin() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("beta"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        let ws = ctx
            .resolve_target(&registry, "beta/main", TargetScope::Any)
            .unwrap();
        assert_eq!(ws.name(), "main");
        assert_eq!(ws.project().name(), "beta");
    }

    #[test]
    fn qualified_target_resolves_a_valid_workspace_name() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("alpha"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        let ws = ctx
            .resolve_target(&registry, "alpha/main", TargetScope::Any)
            .unwrap();
        assert_eq!(ws.name(), "main");
        assert_eq!(ws.project().name(), "alpha");
    }

    #[test]
    fn qualified_target_rejects_invalid_workspace_paths() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("alpha"),
            None,
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        for target in [
            "alpha/.",
            "alpha/..",
            "alpha/bad.name",
            "alpha/main/child",
            "alpha/main/..",
            "alpha/../../beta/worktrees/main",
        ] {
            assert!(matches!(
                ctx.resolve_target(&registry, target, TargetScope::Any),
                Err(Error::WorkspaceNotFound(name)) if name == target
            ));
        }
    }

    #[test]
    fn project_and_workspace_flags_resolve_from_a_foreign_origin() {
        let (_dir, pdir, registry) = two_projects();
        let ctx = Scope::resolve(
            &registry,
            Some("beta"),
            Some("main"),
            &workspace_path(&pdir, "alpha"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ctx.scope.project().name(), "beta");
        assert_eq!(ctx.scope.require_workspace().unwrap().name(), "main");
    }
}
