use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    fs::{flock_exclusive, flock_shared},
    paths,
    settings::GlobalSettings,
};

/// The global subspace state persisted at `~/.local/state/subspace/projects.json`.
///
/// The `projects` map is the source of truth for where projects live. Each name maps to a directory
/// at any absolute path. Project commands maintain this index, so it is state rather than authored
/// config. `#[serde(default)]` on every field keeps the document additive, so a newer subspace can
/// add more global keys without breaking an older file.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct GlobalState {
    /// Registered projects, keyed by name, each mapping to its directory.
    #[serde(default)]
    projects: BTreeMap<String, PathBuf>,
}

impl GlobalState {
    /// Returns the directory registered for the project `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Path> {
        self.projects.get(name).map(PathBuf::as_path)
    }

    /// Iterates the registered projects in name order.
    pub fn all(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.projects
            .iter()
            .map(|(name, path)| (name.as_str(), path.as_path()))
    }

    /// Registers `path` under `name`. Fails with [`Error::ProjectExists`] when the name is taken.
    pub fn insert(&mut self, name: &str, path: PathBuf) -> Result<()> {
        if self.projects.contains_key(name) {
            return Err(Error::ProjectExists(name.to_string()));
        }
        self.projects.insert(name.to_string(), path);
        Ok(())
    }

    /// Drops the registration for `name` and returns its path. Fails with
    /// [`Error::ProjectNotFound`] when the name is not registered.
    pub fn remove(&mut self, name: &str) -> Result<PathBuf> {
        self.projects
            .remove(name)
            .ok_or_else(|| Error::ProjectNotFound(name.to_string()))
    }

    /// Adds every registration from `other` that this document does not already name. A name held
    /// here keeps pointing where it already did.
    fn merge(&mut self, other: GlobalState) {
        for (name, path) in other.projects {
            self.projects.entry(name).or_insert(path);
        }
    }
}

/// This user's settings for the binary itself, within the subspace config directory.
pub const GLOBAL_CONFIG_FILE: &str = "config.json";
/// The directory that held the global documents before the XDG layout, within `$HOME`. Its
/// documents are adopted into their XDG locations on the first run.
pub const LEGACY_GLOBAL_DIR: &str = ".subspace";
/// The project registry within [`LEGACY_GLOBAL_DIR`].
pub const LEGACY_DIR_STATE_FILE: &str = "state.json";
/// The single global document subspace used before `~/.subspace/`. It held the project registry, so
/// it is adopted into the registry on the first run.
pub const LEGACY_GLOBAL_FILE: &str = ".subspace.json";
/// The registry's location before it moved into the `subspace/` state directory, within the state
/// home. It is adopted into `projects.json` on the first run.
pub const LEGACY_FLAT_STATE_FILE: &str = "subspace.json";

/// Handle to the project registry and hook config at their XDG locations.
pub struct Registry {
    state: PathBuf,
    flat_state: PathBuf,
    config: PathBuf,
    legacy_dir: PathBuf,
    legacy: PathBuf,
}

impl Registry {
    /// Creates a handle to the project registry at `state` and hook config at `config`.
    /// `flat_state` names the pre-v11 flat registry file in the state home, `legacy_dir` the
    /// pre-XDG `~/.subspace/` directory, and `legacy` the pre-`~/.subspace/` registry file.
    pub fn new(
        state: PathBuf,
        flat_state: PathBuf,
        config: PathBuf,
        legacy_dir: PathBuf,
        legacy: PathBuf,
    ) -> Registry {
        Registry {
            state,
            flat_state,
            config,
            legacy_dir,
            legacy,
        }
    }

    /// Resolves the registry and hook config through the process XDG directories.
    pub fn from_env() -> Result<Registry> {
        let config_dir = paths::subspace_config_dir()?;
        let legacy_dir = Registry::legacy_dir_at_home()?;
        let home = legacy_dir.parent().unwrap_or(&legacy_dir).to_path_buf();
        Ok(Registry::new(
            paths::global_state_file()?,
            paths::state_home()?.join(LEGACY_FLAT_STATE_FILE),
            config_dir.join(GLOBAL_CONFIG_FILE),
            legacy_dir,
            home.join(LEGACY_GLOBAL_FILE),
        ))
    }

    /// Returns `$HOME/.subspace/`, the directory the global documents lived in before the XDG
    /// layout.
    fn legacy_dir_at_home() -> Result<PathBuf> {
        let home = std::env::var("HOME").map_err(|_| Error::HomeNotSet)?;
        Ok(Path::new(&home).join(LEGACY_GLOBAL_DIR))
    }

    /// Moves registry state and hook config out of the pre-XDG `~/.subspace/` directory.
    fn adopt_legacy_dir(&self) -> Result<()> {
        if !self.legacy_dir.is_dir() {
            return Ok(());
        }
        let old_state = self.legacy_dir.join(LEGACY_DIR_STATE_FILE);
        if old_state.is_file() {
            let content = fs::read_to_string(&old_state)?;
            let legacy: GlobalState = if content.trim().is_empty() {
                GlobalState::default()
            } else {
                serde_json::from_str(&content)?
            };
            let mut current = self.read_state()?;
            current.merge(legacy);
            self.write_state(&current)?;
            fs::remove_file(&old_state)?;
        }
        let old_config = self.legacy_dir.join(GLOBAL_CONFIG_FILE);
        if old_config.exists() && !self.config.exists() {
            fs::rename(old_config, &self.config)?;
        }
        let _ = fs::remove_dir(&self.legacy_dir);
        Ok(())
    }

    /// Folds a pre-v11 flat registry at `<state home>/subspace.json` into `projects.json` and
    /// removes it. Registrations already there win, so adopting twice cannot undo a later `project
    /// add`.
    fn adopt_flat_state(&self) -> Result<()> {
        if !self.flat_state.is_file() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.flat_state)?;
        let legacy: GlobalState = if content.trim().is_empty() {
            GlobalState::default()
        } else {
            serde_json::from_str(&content)?
        };
        let mut current = self.read_state()?;
        current.merge(legacy);
        self.write_state(&current)?;
        fs::remove_file(&self.flat_state)?;
        Ok(())
    }

    /// Folds a pre-`~/.subspace/` registry into the state file and removes it.
    ///
    /// The binary carries this because the registry resolves each project name.
    /// Registrations already in `state.json` win, so adopting twice cannot undo a later `project
    /// add`.
    fn adopt_legacy(&self) -> Result<()> {
        if !self.legacy.is_file() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.legacy)?;
        let legacy: GlobalState = if content.trim().is_empty() {
            GlobalState::default()
        } else {
            serde_json::from_str(&content)?
        };
        let mut current = self.read_state()?;
        current.merge(legacy);
        self.write_state(&current)?;
        fs::remove_file(&self.legacy)?;
        Ok(())
    }

    /// Creates missing registry state and hook config with `{}`, after it
    /// adopts any earlier layout. The flat `<state home>/subspace.json` file is adopted before the
    /// `~/.subspace/` directory, which is adopted before the `~/.subspace.json` file it replaced,
    /// newest layout first, so a machine that skipped every upgrade still folds every registration
    /// in.
    pub fn ensure_exists(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.ensure_exists_unlocked()
    }

    /// Locks the stable registry lock file across replacement of the state file.
    fn lock(&self) -> Result<File> {
        if let Some(parent) = self.state.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.state.with_extension("json.lock"))?;
        flock_exclusive(&file)?;
        Ok(file)
    }

    /// Creates missing documents while the registry lock is held.
    fn ensure_exists_unlocked(&self) -> Result<()> {
        if let Some(parent) = self.state.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = self.config.parent() {
            fs::create_dir_all(parent)?;
        }
        self.adopt_flat_state()?;
        self.adopt_legacy_dir()?;
        self.adopt_legacy()?;
        for path in [&self.state, &self.config] {
            if !path.exists() {
                fs::write(path, "{}\n")?;
            }
        }
        Ok(())
    }

    /// Reads and deserializes `state.json` alone, without creating anything.
    fn read_state(&self) -> Result<GlobalState> {
        let file = match File::open(&self.state) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GlobalState::default());
            }
            Err(e) => return Err(e.into()),
        };
        flock_shared(&file)?;
        let mut content = String::new();
        let mut file = file;
        file.read_to_string(&mut content)?;
        if content.trim().is_empty() {
            return Ok(GlobalState::default());
        }
        Ok(serde_json::from_str(&content)?)
    }

    /// Writes `state` to `state.json` atomically.
    fn write_state(&self, state: &GlobalState) -> Result<()> {
        let mut json = serde_json::to_string_pretty(state)?;
        json.push('\n');
        let parent = self.state.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "registry has no parent")
        })?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(json.as_bytes())?;
        tmp.persist(&self.state).map_err(|error| error.error)?;
        Ok(())
    }

    /// Reads the global state without creating or migrating files.
    pub fn read(&self) -> Result<GlobalState> {
        self.read_state()
    }

    /// Applies `f` to the global state and writes it back atomically. Nothing is written when `f`
    /// fails.
    pub fn update(&self, f: impl FnOnce(&mut GlobalState) -> Result<()>) -> Result<()> {
        let _lock = self.lock()?;
        self.ensure_exists_unlocked()?;
        let mut state = self.read_state()?;
        f(&mut state)?;
        self.write_state(&state)
    }

    /// Reads `config.json`, this user's settings for what subspace runs after a mutation. A
    /// document that is absent or empty reads as the defaults, so a machine that authored nothing
    /// behaves as one that authored every default.
    ///
    /// The document is validated here rather than at the point of use, so a typo in an event name
    /// or a placeholder is reported once, at the start of the run, instead of when a hook runs.
    /// Every failure names the file: this runs on every command, so an unattributed parse error
    /// follows the user around with nothing to act on.
    ///
    /// An absent document reads as the defaults without creating files. Other read failures are
    /// reported because an unreadable config must not become an empty config.
    pub fn read_settings(&self) -> Result<GlobalSettings> {
        let content = match fs::read_to_string(&self.config) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(Error::InvalidSettings {
                    path: self.config.clone(),
                    message: error.to_string(),
                });
            }
        };
        if content.trim().is_empty() {
            return Ok(GlobalSettings::default());
        }
        serde_json::from_str(&content).map_err(|error| Error::InvalidSettings {
            path: self.config.clone(),
            message: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_at(root: &Path) -> Registry {
        let config = root.join(".config/subspace");
        Registry::new(
            root.join(".local/state/subspace/projects.json"),
            root.join(".local/state").join(LEGACY_FLAT_STATE_FILE),
            config.join(GLOBAL_CONFIG_FILE),
            root.join(LEGACY_GLOBAL_DIR),
            root.join(LEGACY_GLOBAL_FILE),
        )
    }

    fn registry_file() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_at(dir.path());
        (dir, registry)
    }

    #[test]
    fn registry_update_child() {
        let Ok(root) = std::env::var("SUBSPACE_REGISTRY_TEST_ROOT") else {
            return;
        };
        registry_at(Path::new(&root))
            .update(|state| state.insert("child", PathBuf::from("/child")))
            .unwrap();
    }

    #[test]
    fn separate_process_updates_keep_every_project() {
        let (dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        let mut child = None;
        registry
            .update(|state| {
                state.insert("parent", PathBuf::from("/parent"))?;
                child = Some(
                    std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "registry::tests::registry_update_child"])
                        .env("SUBSPACE_REGISTRY_TEST_ROOT", dir.path())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()?,
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
                Ok(())
            })
            .unwrap();
        let output = child.unwrap().wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let state = registry.read().unwrap();
        assert_eq!(state.get("parent"), Some(Path::new("/parent")));
        assert_eq!(state.get("child"), Some(Path::new("/child")));
    }

    #[cfg(unix)]
    #[test]
    fn registry_write_does_not_follow_a_temp_symlink() {
        let (dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        let sentinel = dir.path().join("sentinel");
        fs::write(&sentinel, "keep").unwrap();
        let tmp = registry.state.with_extension("json.tmp");
        std::os::unix::fs::symlink(&sentinel, &tmp).unwrap();
        registry
            .update(|state| state.insert("alpha", PathBuf::from("/alpha")))
            .unwrap();
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
        assert!(tmp.is_symlink());
        assert_eq!(
            registry.read().unwrap().get("alpha"),
            Some(Path::new("/alpha"))
        );
    }

    #[test]
    fn registry_write_does_not_follow_a_temp_hardlink() {
        let (dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        let sentinel = dir.path().join("sentinel");
        fs::write(&sentinel, "keep").unwrap();
        let tmp = registry.state.with_extension("json.tmp");
        fs::hard_link(&sentinel, &tmp).unwrap();
        registry
            .update(|state| state.insert("alpha", PathBuf::from("/alpha")))
            .unwrap();
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
        assert_eq!(fs::read_to_string(tmp).unwrap(), "keep");
        assert_eq!(
            registry.read().unwrap().get("alpha"),
            Some(Path::new("/alpha"))
        );
    }

    #[test]
    fn concurrent_registry_updates_keep_every_project() {
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        let registry = std::sync::Arc::new(registry);
        let start = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let registry = registry.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    let name = format!("project{index}");
                    registry.update(|state| {
                        state.insert(&name, PathBuf::from(format!("/tmp/{name}")))?;
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok(())
                    })
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(registry.read().unwrap().all().count(), 4);
    }

    #[test]
    fn reads_do_not_create_registry_or_config() {
        let (_dir, registry) = registry_file();
        assert_eq!(registry.read().unwrap().all().count(), 0);
        assert_eq!(registry.read_settings().unwrap(), GlobalSettings::default());
        assert!(!registry.state.exists());
        assert!(!registry.config.exists());
        assert!(!registry.state.with_extension("json.lock").exists());
    }

    #[test]
    fn absent_file_reads_as_empty() {
        let (_dir, registry) = registry_file();
        assert_eq!(registry.read().unwrap().all().count(), 0);
    }

    #[test]
    fn ensure_exists_creates_registry_and_hook_config() {
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        for path in [&registry.state, &registry.config] {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert!(value.as_object().is_some_and(|value| value.is_empty()));
        }
    }

    #[test]
    fn an_absent_config_reads_the_default_settings() {
        let (_dir, registry) = registry_file();
        assert_eq!(registry.read_settings().unwrap(), GlobalSettings::default());
    }

    #[test]
    fn settings_read_through_the_registry() {
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        std::fs::write(
            &registry.config,
            r#"{"hooks":{"workspace_add_post":"init {workspace}"}}"#,
        )
        .unwrap();
        let settings = registry.read_settings().unwrap();
        assert_eq!(
            settings.hooks().command(crate::hook::Event::new(
                crate::hook::Kind::WorkspaceAdd,
                crate::hook::Phase::Post
            )),
            Some("init {workspace}")
        );
    }

    #[test]
    fn an_unknown_key_fails_the_read() {
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        std::fs::write(&registry.config, r#"{"terminal":"kitty"}"#).unwrap();
        assert!(registry.read_settings().is_err());
    }

    #[test]
    fn settings_are_not_state() {
        // config.json is authored and state.json is not, so a settings key in the state document is
        // an unknown key there: read, ignored, and gone on the next write.
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        std::fs::write(&registry.state, r#"{"projects":{},"hooks":{}}"#).unwrap();
        assert_eq!(registry.read_settings().unwrap(), GlobalSettings::default());
    }

    #[test]
    fn a_legacy_registry_is_adopted_into_state() {
        let (_dir, registry) = registry_file();
        std::fs::write(&registry.legacy, r#"{"projects":{"tide":"/c"}}"#).unwrap();
        registry.ensure_exists().unwrap();

        assert_eq!(registry.read().unwrap().get("tide"), Some(Path::new("/c")));
        assert!(!registry.legacy.exists());
        assert!(
            std::fs::read_to_string(&registry.state)
                .unwrap()
                .contains("/c")
        );
    }

    #[test]
    fn adoption_survives_an_empty_state_written_first() {
        // The upgrade order that matters: some earlier command creates an empty state.json before
        // the legacy file is ever looked at. Adoption must still fold it in rather than treat the
        // new document as authoritative.
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        assert_eq!(registry.read().unwrap().all().count(), 0);

        std::fs::write(&registry.legacy, r#"{"projects":{"tide":"/c"}}"#).unwrap();
        registry.ensure_exists().unwrap();

        assert_eq!(registry.read().unwrap().get("tide"), Some(Path::new("/c")));
        assert!(!registry.legacy.exists());
    }

    #[test]
    fn adoption_keeps_a_registration_state_already_holds() {
        let (_dir, registry) = registry_file();
        registry
            .update(|state| state.insert("tide", PathBuf::from("/new")))
            .unwrap();
        std::fs::write(
            &registry.legacy,
            r#"{"projects":{"tide":"/old","chisel":"/srv/chisel"}}"#,
        )
        .unwrap();

        registry.ensure_exists().unwrap();
        let state = registry.read().unwrap();
        assert_eq!(state.get("tide"), Some(Path::new("/new")));
        assert_eq!(state.get("chisel"), Some(Path::new("/srv/chisel")));
    }

    /// Seeds a pre-v11 flat registry at `registry`'s flat-state path.
    fn seed_flat_state(registry: &Registry, content: &str) {
        std::fs::create_dir_all(registry.flat_state.parent().unwrap()).unwrap();
        std::fs::write(&registry.flat_state, content).unwrap();
    }

    #[test]
    fn a_flat_registry_is_adopted_into_projects_json() {
        let (_dir, registry) = registry_file();
        seed_flat_state(&registry, r#"{"projects":{"tide":"/c"}}"#);
        registry.ensure_exists().unwrap();

        assert_eq!(registry.read().unwrap().get("tide"), Some(Path::new("/c")));
        assert!(!registry.flat_state.exists());
        assert!(
            std::fs::read_to_string(&registry.state)
                .unwrap()
                .contains("/c")
        );
    }

    #[test]
    fn flat_adoption_keeps_a_registration_state_already_holds() {
        let (_dir, registry) = registry_file();
        registry
            .update(|state| state.insert("tide", PathBuf::from("/new")))
            .unwrap();
        seed_flat_state(
            &registry,
            r#"{"projects":{"tide":"/old","chisel":"/srv/chisel"}}"#,
        );
        registry.ensure_exists().unwrap();

        let state = registry.read().unwrap();
        assert_eq!(state.get("tide"), Some(Path::new("/new")));
        assert_eq!(state.get("chisel"), Some(Path::new("/srv/chisel")));
        assert!(!registry.flat_state.exists());
    }

    #[test]
    fn flat_adoption_runs_again_without_effect() {
        let (_dir, registry) = registry_file();
        seed_flat_state(&registry, r#"{"projects":{"tide":"/c"}}"#);
        registry.ensure_exists().unwrap();

        registry.ensure_exists().unwrap();
        assert_eq!(registry.read().unwrap().get("tide"), Some(Path::new("/c")));
        assert!(!registry.flat_state.exists());
    }

    /// Seeds legacy registry state and hook config beside `registry`'s legacy path.
    fn seed_legacy_dir(registry: &Registry) {
        let dir = &registry.legacy_dir;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(LEGACY_DIR_STATE_FILE),
            r#"{"projects":{"tide":"/old"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(GLOBAL_CONFIG_FILE), r#"{"hooks":{}}"#).unwrap();
    }

    #[test]
    fn the_legacy_directory_moves_registry_and_hook_config() {
        let (_dir, registry) = registry_file();
        seed_legacy_dir(&registry);
        registry.ensure_exists().unwrap();

        assert_eq!(
            registry.read().unwrap().get("tide"),
            Some(Path::new("/old"))
        );
        assert_eq!(
            std::fs::read_to_string(&registry.config).unwrap(),
            r#"{"hooks":{}}"#
        );
        assert!(!registry.legacy_dir.exists());
    }

    #[test]
    fn legacy_dir_adoption_keeps_a_registration_state_already_holds() {
        let (_dir, registry) = registry_file();
        registry
            .update(|state| state.insert("tide", PathBuf::from("/new")))
            .unwrap();
        std::fs::create_dir_all(&registry.legacy_dir).unwrap();
        std::fs::write(
            registry.legacy_dir.join(LEGACY_DIR_STATE_FILE),
            r#"{"projects":{"tide":"/old","chisel":"/srv/chisel"}}"#,
        )
        .unwrap();

        registry.ensure_exists().unwrap();
        let state = registry.read().unwrap();
        assert_eq!(state.get("tide"), Some(Path::new("/new")));
        assert_eq!(state.get("chisel"), Some(Path::new("/srv/chisel")));
        assert!(!registry.legacy_dir.exists());
    }

    #[test]
    fn legacy_dir_adoption_leaves_other_files_in_place() {
        let (_dir, registry) = registry_file();
        let global = registry.legacy_dir.join("env.json");
        std::fs::create_dir_all(&registry.legacy_dir).unwrap();
        std::fs::write(&global, r#"{"K":"old"}"#).unwrap();

        registry.ensure_exists().unwrap();
        assert_eq!(std::fs::read_to_string(&global).unwrap(), r#"{"K":"old"}"#);
        assert!(registry.legacy_dir.is_dir());
        assert!(!registry.config.parent().unwrap().join("env.json").exists());
    }

    #[test]
    fn legacy_dir_adoption_runs_again_without_effect() {
        let (_dir, registry) = registry_file();
        seed_legacy_dir(&registry);
        registry.ensure_exists().unwrap();

        registry.ensure_exists().unwrap();
        assert_eq!(
            registry.read().unwrap().get("tide"),
            Some(Path::new("/old"))
        );
        assert_eq!(
            std::fs::read_to_string(&registry.config).unwrap(),
            r#"{"hooks":{}}"#
        );
        assert!(!registry.legacy_dir.exists());
    }

    #[test]
    fn registry_round_trips() {
        let (_dir, registry) = registry_file();
        registry
            .update(|state| state.insert("tide", PathBuf::from("/Users/tmckay/Projects/tide")))
            .unwrap();

        let state = registry.read().unwrap();
        assert_eq!(
            state.get("tide"),
            Some(Path::new("/Users/tmckay/Projects/tide"))
        );
    }

    #[test]
    fn all_yields_names_in_order() {
        let (_dir, registry) = registry_file();
        registry
            .update(|state| {
                state.insert("chisel", PathBuf::from("/srv/chisel"))?;
                state.insert("tide", PathBuf::from("/home/tide"))
            })
            .unwrap();

        let state = registry.read().unwrap();
        let names: Vec<&str> = state.all().map(|(name, _)| name).collect();
        assert_eq!(names, ["chisel", "tide"]);
    }

    #[test]
    fn insert_refuses_a_duplicate_name() {
        let mut state = GlobalState::default();
        state.insert("tide", PathBuf::from("/one")).unwrap();
        assert!(matches!(
            state.insert("tide", PathBuf::from("/two")),
            Err(Error::ProjectExists(_))
        ));
    }

    #[test]
    fn remove_reports_unknown_names() {
        let mut state = GlobalState::default();
        assert!(matches!(
            state.remove("tide"),
            Err(Error::ProjectNotFound(_))
        ));
    }

    #[test]
    fn unknown_keys_are_preserved_as_additive() {
        let (_dir, registry) = registry_file();
        registry.ensure_exists().unwrap();
        std::fs::write(
            &registry.state,
            r#"{"projects":{"tide":"/c"},"future":{"key":1}}"#,
        )
        .unwrap();
        // A document carrying a not-yet-known top-level key still deserializes.
        let state = registry.read().unwrap();
        assert_eq!(state.get("tide"), Some(Path::new("/c")));
    }
}
