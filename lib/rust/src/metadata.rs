use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    fs::{flock_exclusive, flock_shared},
    project::ProjectLock,
    revision::Sha,
};

/// The schema version this build of subspace expects to find in `workspaces.json`. It versions the
/// project layout as a whole, both metadata documents included.
pub const CURRENT_VERSION: u64 = 11;

/// A project's metadata, joined from its two documents.
///
/// The split is by authorship. [`ProjectConfig`] holds what a person decides and a `subspace`
/// command writes for them. [`ProjectState`] holds what subspace derives and maintains. Callers see
/// one struct: the halves are a storage detail of [`Metadata`].
#[derive(Debug, Clone)]
pub struct ProjectMeta {
    /// Schema version.
    pub version: u64,
    /// Name of the default workspace, if any.
    pub default: Option<String>,
    /// Prefix prepended to auto-derived branch names (`<prefix>/<name>`).
    pub branch_prefix: Option<String>,
    /// Per-workspace metadata keyed by workspace name.
    pub workspaces: HashMap<String, WorkspaceMeta>,
}

impl Default for ProjectMeta {
    fn default() -> ProjectMeta {
        ProjectMeta {
            version: CURRENT_VERSION,
            default: None,
            branch_prefix: None,
            workspaces: HashMap::new(),
        }
    }
}

/// The half of a project's metadata a person decides, persisted to `config/subspace.json`, in the
/// config directory with the project's other authored files. `subspace default` and `subspace
/// branch-prefix` write it.
#[derive(Debug, Default, Deserialize, Serialize)]
struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_prefix: Option<String>,
}

/// The half of a project's metadata subspace derives and maintains, persisted to `workspaces.json`
/// in the state directory. It carries the schema version, so it is the document a stale build is
/// caught on.
#[derive(Debug, Deserialize, Serialize)]
struct ProjectState {
    #[serde(default)]
    version: u64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    workspaces: HashMap<String, WorkspaceMeta>,
}

impl ProjectMeta {
    /// Joins the two persisted documents into one [`ProjectMeta`].
    fn join(config: ProjectConfig, state: ProjectState) -> ProjectMeta {
        ProjectMeta {
            version: state.version,
            default: config.default,
            branch_prefix: config.branch_prefix,
            workspaces: state.workspaces,
        }
    }

    /// Splits this metadata back into the documents each half persists to.
    fn split(self) -> (ProjectConfig, ProjectState) {
        (
            ProjectConfig {
                default: self.default,
                branch_prefix: self.branch_prefix,
            },
            ProjectState {
                version: self.version,
                workspaces: self.workspaces,
            },
        )
    }
}

impl ProjectMeta {
    /// Builds a child-to-parent edge map, limited to names present in `known`.
    pub fn edges(&self, known: &HashSet<&str>) -> HashMap<String, String> {
        let mut edges = HashMap::new();
        for (name, ws) in &self.workspaces {
            if let Some(parent) = &ws.parent
                && known.contains(name.as_str())
                && known.contains(parent.name.as_str())
            {
                edges.insert(name.clone(), parent.name.clone());
            }
        }
        edges
    }

    /// Renames a workspace entry and updates all parent references and the default.
    pub fn rename_workspace(&mut self, old_name: &str, new_name: &str) {
        if let Some(meta) = self.workspaces.remove(old_name) {
            self.workspaces.insert(new_name.to_string(), meta);
        }
        for ws_meta in self.workspaces.values_mut() {
            if let Some(parent) = &mut ws_meta.parent
                && parent.name == old_name
            {
                parent.name = new_name.to_string();
            }
        }
        if self.default.as_deref() == Some(old_name) {
            self.default = Some(new_name.to_string());
        }
    }
}

/// A workspace's parent edge: the parent's name paired with the base commit the child branched
/// from. A child always carries both. A root workspace has neither.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ParentEdge {
    /// Name of the parent workspace.
    pub name: String,
    /// The commit the child's stack is rooted at (its branch point).
    pub base: Sha,
}

/// Persisted metadata for a single workspace within a project.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct WorkspaceMeta {
    /// The parent edge, if this workspace has a parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentEdge>,
    /// Git branch correlated with this workspace, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The integration state needed to continue or abort a stopped operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<IntegrationState>,
}

/// The original inputs of an active merge or rebase.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrationState {
    /// The original base workspace and commit.
    pub original_base: Option<ParentEdge>,
    /// The requested target workspace and commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ParentEdge>,
    /// The integration mode.
    pub mode: String,
    /// The squash message, when the merge uses squash mode.
    pub squash_message: Option<String>,
}

/// Handle to a project's two metadata documents, `config/subspace.json` and `workspaces.json`.
///
/// Callers serialize mutations through `Project::lock`. The per-file flock in `read`/`update` is
/// defense in depth against concurrent readers. Both files are always locked config-first, so two
/// invocations cannot deadlock against each other.
pub struct Metadata {
    config: PathBuf,
    state: PathBuf,
    legacy: Vec<PathBuf>,
}

/// Deserializes one metadata document from file `content`. Empty content yields the default.
fn parse<T: serde::de::DeserializeOwned + Default>(content: &str) -> Result<T> {
    if content.trim().is_empty() {
        return Ok(T::default());
    }
    Ok(serde_json::from_str(content)?)
}

impl Default for ProjectState {
    fn default() -> ProjectState {
        ProjectState {
            version: CURRENT_VERSION,
            workspaces: HashMap::new(),
        }
    }
}

/// Reads `path` under a shared flock. Returns an empty string when it is absent.
fn read_locked(path: &PathBuf) -> Result<String> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.into()),
    };
    flock_shared(&file)?;
    let mut content = String::new();
    let mut file = file;
    file.read_to_string(&mut content)?;
    Ok(content)
}

/// Opens `path` for a read-modify-write cycle under an exclusive flock. Creates it and its parent
/// directory when absent, and returns the handle with the content it held.
fn open_locked(path: &PathBuf) -> Result<(File, String)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    flock_exclusive(&file)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok((file, content))
}

/// Rewrites `file` with `json` when it differs from the `previous` content, so a mutation that
/// touches only one document leaves the other's mtime alone.
fn write_if_changed(file: &mut File, previous: &str, json: &str) -> Result<()> {
    if previous == json {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

impl Metadata {
    /// Creates a handle to the metadata documents at `config` and `state`. `legacy` names the
    /// derived document of each earlier layout, newest first.
    pub fn new(config: PathBuf, state: PathBuf, legacy: Vec<PathBuf>) -> Metadata {
        Metadata {
            config,
            state,
            legacy,
        }
    }

    /// Reports the project as unmigrated when it has no `workspaces.json` in its state directory
    /// but still carries the derived document of an earlier layout: a v8-v10 `.workspaces.json` at
    /// the project root, a v6 `.subspace/state.json`, or a pre-v6 `.project.json`.
    ///
    /// Without this, an unmigrated project reads as an empty default rather than fail, because an
    /// absent document reads as its default. Such a read answers a question about the workspace
    /// tree with silence, rather than report the unsupported schema.
    fn check_not_legacy(&self, state: &str) -> Result<()> {
        if !state.trim().is_empty() {
            return Ok(());
        }
        let Some(legacy) = self.legacy.iter().find(|path| path.is_file()) else {
            return Ok(());
        };
        let found = std::fs::read_to_string(legacy)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| v.get("version").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        Err(Error::VersionMismatch {
            found,
            expected: CURRENT_VERSION,
        })
    }

    /// Joins the two documents and checks the schema version `state` carries.
    fn join(config: &str, state: &str) -> Result<ProjectMeta> {
        let config: ProjectConfig = parse(config)?;
        let state: ProjectState = parse(state)?;
        if state.version != CURRENT_VERSION {
            return Err(Error::VersionMismatch {
                found: state.version,
                expected: CURRENT_VERSION,
            });
        }
        Ok(ProjectMeta::join(config, state))
    }

    /// Serializes `meta` back into the text of each document, newline included.
    fn render(meta: ProjectMeta) -> Result<(String, String)> {
        let (config, state) = meta.split();
        Ok((
            serde_json::to_string_pretty(&config)? + "\n",
            serde_json::to_string_pretty(&state)? + "\n",
        ))
    }

    /// Reads and joins the project metadata under a shared flock on each document. A document that
    /// does not exist or is empty reads as its default.
    pub fn read(&self) -> Result<ProjectMeta> {
        let config = read_locked(&self.config)?;
        let state = read_locked(&self.state)?;
        self.check_not_legacy(&state)?;
        Metadata::join(&config, &state)
    }

    /// Applies `f` to the project metadata under an exclusive flock on each document, then writes
    /// back whichever of the two changed.
    ///
    /// If `f` returns `Err`, nothing is written and the error propagates. The flocks are held for
    /// the whole read-modify-write cycle, so a caller can make a mutation that depends on a
    /// consistent view.
    pub fn update(
        &self,
        _lock: &ProjectLock,
        f: impl FnOnce(&mut ProjectMeta) -> Result<()>,
    ) -> Result<()> {
        // A stat rather than a read, and taken before either document is opened: `open_locked`
        // creates what it opens, so an unmigrated project must be refused before an empty
        // `workspaces.json` is left behind.
        if !self.state.exists() {
            self.check_not_legacy("")?;
        }
        let (mut config_file, config_before) = open_locked(&self.config)?;
        let (mut state_file, state_before) = open_locked(&self.state)?;
        let mut data = Metadata::join(&config_before, &state_before)?;
        f(&mut data)?;
        let (config_after, state_after) = Metadata::render(data)?;
        write_if_changed(&mut config_file, &config_before, &config_after)?;
        write_if_changed(&mut state_file, &state_before, &state_after)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(hex: &str) -> Sha {
        Sha::new(git2::Oid::from_str(hex).unwrap())
    }

    /// Creates a project directory with `config/subspace.json` and `workspaces.json` at the given
    /// content. Returns the tempdir and a metadata handle on the pair.
    fn metadata_files(config: &str, state: &str) -> (tempfile::TempDir, Metadata) {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::project::Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let config_path = project.metadata_config_path();
        let state_path = project.metadata_state_path();
        for path in [&config_path, &state_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&config_path, config).unwrap();
        std::fs::write(&state_path, state).unwrap();
        (dir, project.metadata())
    }

    /// As [`metadata_files`], with an empty config document.
    fn metadata_file(state: &str) -> (tempfile::TempDir, Metadata) {
        metadata_files("", state)
    }

    #[test]
    fn read_rejects_stale_version() {
        let (_dir, metadata) = metadata_file(r#"{"version": 0}"#);
        match metadata.read() {
            Err(Error::VersionMismatch { found, expected }) => {
                assert_eq!(found, 0);
                assert_eq!(expected, CURRENT_VERSION);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn read_rejects_missing_version_field() {
        let (_dir, metadata) = metadata_file(r#"{"workspaces": {}}"#);
        match metadata.read() {
            Err(Error::VersionMismatch { found, .. }) => assert_eq!(found, 0),
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn read_accepts_current_version() {
        assert_eq!(ProjectMeta::default().version, CURRENT_VERSION);
        let (_dir, metadata) = metadata_file(&format!(r#"{{"version": {CURRENT_VERSION}}}"#));
        let meta = metadata.read().unwrap();
        assert_eq!(meta.version, CURRENT_VERSION);
    }

    #[test]
    fn update_rejects_stale_version() {
        let (dir, metadata) = metadata_file(r#"{"version": 0}"#);
        let project = crate::project::Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let lock = project.lock("test").unwrap();
        match metadata.update(&lock, |_| Ok(())) {
            Err(Error::VersionMismatch { found, .. }) => assert_eq!(found, 0),
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    /// Renders `meta` to its two documents and joins them back.
    fn round_trip(meta: ProjectMeta) -> (String, String, ProjectMeta) {
        let (config, state) = Metadata::render(meta).unwrap();
        let back = Metadata::join(&config, &state).unwrap();
        (config, state, back)
    }

    #[test]
    fn parent_round_trips_as_object() {
        let mut meta = ProjectMeta::default();
        meta.workspaces.insert(
            "child".to_string(),
            WorkspaceMeta {
                parent: Some(ParentEdge {
                    name: "root".to_string(),
                    base: sha("abc123"),
                }),
                branch: None,
                integration: None,
            },
        );
        meta.workspaces
            .insert("root".to_string(), WorkspaceMeta::default());
        let (_, _, back) = round_trip(meta);
        assert_eq!(
            back.workspaces["child"].parent,
            Some(ParentEdge {
                name: "root".to_string(),
                base: sha("abc123"),
            })
        );
        assert_eq!(back.workspaces["root"].parent, None);
    }

    #[test]
    fn integration_state_round_trips_original_base_target_and_mode() {
        let mut meta = ProjectMeta::default();
        meta.workspaces.insert(
            "child".to_string(),
            WorkspaceMeta {
                parent: None,
                branch: None,
                integration: Some(IntegrationState {
                    original_base: Some(ParentEdge {
                        name: "old".to_string(),
                        base: sha("1111111111111111111111111111111111111111"),
                    }),
                    target: Some(ParentEdge {
                        name: "new".to_string(),
                        base: sha("2222222222222222222222222222222222222222"),
                    }),
                    mode: "rebase:interactive=false,all=true".to_string(),
                    squash_message: None,
                }),
            },
        );
        let (_, _, back) = round_trip(meta);
        let integration = back.workspaces["child"].integration.as_ref().unwrap();
        assert_eq!(integration.target.as_ref().unwrap().name, "new");
        assert_eq!(integration.mode, "rebase:interactive=false,all=true");
    }

    #[test]
    fn an_empty_project_renders_two_empty_documents() {
        let (config, state, _) = round_trip(ProjectMeta::default());
        assert_eq!(config, "{}\n");
        assert_eq!(state, format!("{{\n  \"version\": {CURRENT_VERSION}\n}}\n"));
    }

    #[test]
    fn config_holds_the_authored_keys_and_state_holds_the_derived_ones() {
        let mut meta = ProjectMeta::default();
        meta.default = Some("main".to_string());
        meta.branch_prefix = Some("wip".to_string());
        meta.workspaces
            .insert("main".to_string(), WorkspaceMeta::default());

        let (config, state, back) = round_trip(meta);
        for key in ["default", "branch_prefix"] {
            assert!(config.contains(key), "config.json lacks {key}: {config}");
            assert!(!state.contains(key), "state.json carries {key}: {state}");
        }
        for key in ["version", "workspaces"] {
            assert!(state.contains(key), "state.json lacks {key}: {state}");
            assert!(!config.contains(key), "config.json carries {key}: {config}");
        }

        assert_eq!(back.default.as_deref(), Some("main"));
        assert_eq!(back.branch_prefix.as_deref(), Some("wip"));
        assert!(back.workspaces.contains_key("main"));
    }

    #[test]
    fn update_leaves_the_untouched_document_alone() {
        let config_before = "{\n  \"default\": \"main\"\n}\n";
        let (dir, metadata) = metadata_files(
            config_before,
            &format!(r#"{{"version":{CURRENT_VERSION}}}"#),
        );
        let project = crate::project::Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let lock = project.lock("test").unwrap();
        let config_path = dir.path().join("proj/config/subspace.json");
        let before = std::fs::metadata(&config_path).unwrap().modified().unwrap();

        metadata
            .update(&lock, |meta| {
                meta.workspaces
                    .insert("main".to_string(), WorkspaceMeta::default());
                Ok(())
            })
            .unwrap();

        // Only `workspaces` moved, so config.json is not rewritten at all.
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            config_before
        );
        assert_eq!(
            std::fs::metadata(&config_path).unwrap().modified().unwrap(),
            before
        );
        let meta = metadata.read().unwrap();
        assert_eq!(meta.default.as_deref(), Some("main"));
        assert!(meta.workspaces.contains_key("main"));
    }

    /// Creates a project with only the legacy document at `relative`, at the given content.
    fn legacy_project(
        relative: &str,
        content: &str,
    ) -> (tempfile::TempDir, crate::project::Project) {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::project::Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        let path = project.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        (dir, project)
    }

    /// Asserts that `project` is refused as unmigrated at version `found`, by both a read and an
    /// update, and that the refused update writes nothing.
    fn assert_unmigrated(project: &crate::project::Project, found_version: u64) {
        match project.metadata().read() {
            Err(Error::VersionMismatch { found, expected }) => {
                assert_eq!(found, found_version);
                assert_eq!(expected, CURRENT_VERSION);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }

        let lock = project.lock("test").unwrap();
        assert!(matches!(
            project.metadata().update(&lock, |_| Ok(())),
            Err(Error::VersionMismatch { .. })
        ));
        assert!(!project.metadata_state_path().exists());
    }

    #[test]
    fn a_pre_v6_project_is_refused_rather_than_read_as_empty() {
        // One .project.json, and neither of the documents that replaced it.
        let (_dir, project) = legacy_project(
            crate::project::LEGACY_METADATA_FILE,
            r#"{"version": 5, "default": "main"}"#,
        );
        assert_unmigrated(&project, 5);
    }

    #[test]
    fn a_v10_project_is_refused_rather_than_read_as_empty() {
        // The derived half still at the project root, where it sat through v10, and no
        // workspaces.json in the state directory.
        let (_dir, project) =
            legacy_project(crate::project::LEGACY_ROOT_STATE_FILE, r#"{"version": 10}"#);
        assert_unmigrated(&project, 10);
    }

    #[test]
    fn a_v7_project_is_refused_rather_than_read_as_empty() {
        // The derived half still in the directory it sat in through v7.
        let legacy = format!(
            "{}/{}",
            crate::project::LEGACY_METADATA_DIR,
            crate::project::LEGACY_METADATA_STATE_FILE
        );
        let (_dir, project) = legacy_project(&legacy, r#"{"version": 7}"#);
        assert_unmigrated(&project, 7);
    }

    #[test]
    fn update_creates_both_documents_when_neither_exists() {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::project::Project::with_dirs(
            "proj".to_string(),
            dir.path().join("proj"),
            dir.path().join("proj").join("config"),
            dir.path().join("state"),
        );
        std::fs::create_dir_all(project.path()).unwrap();
        let lock = project.lock("test").unwrap();

        project
            .metadata()
            .update(&lock, |meta| {
                meta.default = Some("main".to_string());
                Ok(())
            })
            .unwrap();

        assert!(project.metadata_config_path().is_file());
        assert!(project.metadata_state_path().is_file());
        assert_eq!(project.metadata().read().unwrap().version, CURRENT_VERSION);
    }

    #[test]
    fn rename_workspace_preserves_base() {
        let mut meta = ProjectMeta::default();
        meta.workspaces.insert(
            "child".to_string(),
            WorkspaceMeta {
                parent: Some(ParentEdge {
                    name: "old".to_string(),
                    base: sha("deadbeef"),
                }),
                branch: None,
                integration: None,
            },
        );
        meta.rename_workspace("old", "new");
        assert_eq!(
            meta.workspaces["child"].parent,
            Some(ParentEdge {
                name: "new".to_string(),
                base: sha("deadbeef"),
            })
        );
    }
}
