use std::{
    env,
    path::{Path, PathBuf},
};

use crate::error::{Error, Result};

#[cfg(test)]
thread_local! {
    /// The home directory [`set_test_home`] has pointed this thread at.
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Points every base directory this module resolves below `home`, for the calling thread only.
/// Tests share one process, so the environment cannot carry a per-test home, but each test runs on
/// its own thread, so a thread-local can. Production code on another thread never sees an override.
#[cfg(test)]
pub(crate) fn set_test_home(home: &Path) {
    TEST_HOME.with(|cell| *cell.borrow_mut() = Some(home.to_path_buf()));
}

/// Returns the home directory [`set_test_home`] pointed this thread at, if any.
#[cfg(test)]
fn test_home() -> Option<PathBuf> {
    TEST_HOME.with(|cell| cell.borrow().clone())
}

/// Returns the XDG config home: `$XDG_CONFIG_HOME` when it is set, non-empty, and absolute, else
/// `$HOME/.config`.
pub fn config_home() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(home) = test_home() {
        return Ok(home.join(".config"));
    }
    base_dir("XDG_CONFIG_HOME", ".config")
}

/// Returns the XDG state home: `$XDG_STATE_HOME` when it is set, non-empty, and absolute, else
/// `$HOME/.local/state`.
pub fn state_home() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(home) = test_home() {
        return Ok(home.join(".local/state"));
    }
    base_dir("XDG_STATE_HOME", ".local/state")
}

/// Returns the directory that holds subspace's global documents, `<config home>/subspace/`.
pub fn subspace_config_dir() -> Result<PathBuf> {
    Ok(config_home()?.join("subspace"))
}

/// Returns the config directory of the project `name`, `<config home>/subspace/projects/<name>/`.
/// The directory is keyed by the project name, so it resolves without opening the project.
pub fn project_config_dir(name: &str) -> Result<PathBuf> {
    Ok(subspace_config_dir()?.join("projects").join(name))
}

/// Returns the directory that holds subspace's derived state, `<state home>/subspace/`.
pub fn subspace_state_dir() -> Result<PathBuf> {
    Ok(state_home()?.join("subspace"))
}

/// Returns the state directory of the project `name`, `<state home>/subspace/projects/<name>/`. The
/// directory is keyed by the project name, like [`project_config_dir`], so it resolves without
/// opening the project.
pub fn project_state_dir(name: &str) -> Result<PathBuf> {
    Ok(subspace_state_dir()?.join("projects").join(name))
}

/// Returns the path of the global project registry, `<state home>/subspace/projects.json`.
pub fn global_state_file() -> Result<PathBuf> {
    Ok(subspace_state_dir()?.join("projects.json"))
}

/// Resolves one XDG base directory from the process environment: `var` when it qualifies, else
/// `$HOME/<fallback>`.
fn base_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    resolve(
        env::var(var).ok().as_deref(),
        env::var("HOME").ok().as_deref(),
        fallback,
    )
}

/// Resolves one XDG base directory from explicit values: `var` when it is set, non-empty, and
/// absolute, else `home`/`fallback`. The XDG specification says to ignore a relative or empty
/// value. [`Error::HomeNotSet`] is reported only when the fallback is needed.
fn resolve(var: Option<&str>, home: Option<&str>, fallback: &str) -> Result<PathBuf> {
    if let Some(dir) = var {
        let path = Path::new(dir);
        if !dir.is_empty() && path.is_absolute() {
            return Ok(path.to_path_buf());
        }
    }
    let home = home.ok_or(Error::HomeNotSet)?;
    Ok(Path::new(home).join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_variable_wins() {
        assert_eq!(
            resolve(Some("/xdg/config"), Some("/home/me"), ".config").unwrap(),
            PathBuf::from("/xdg/config")
        );
    }

    #[test]
    fn an_unset_variable_falls_back_to_home() {
        assert_eq!(
            resolve(None, Some("/home/me"), ".config").unwrap(),
            PathBuf::from("/home/me/.config")
        );
    }

    #[test]
    fn an_empty_variable_falls_back_to_home() {
        assert_eq!(
            resolve(Some(""), Some("/home/me"), ".local/state").unwrap(),
            PathBuf::from("/home/me/.local/state")
        );
    }

    #[test]
    fn a_relative_variable_falls_back_to_home() {
        assert_eq!(
            resolve(Some("config"), Some("/home/me"), ".config").unwrap(),
            PathBuf::from("/home/me/.config")
        );
    }

    #[test]
    fn the_state_paths_mirror_the_config_paths() {
        set_test_home(Path::new("/home/me"));
        assert_eq!(
            subspace_state_dir().unwrap(),
            PathBuf::from("/home/me/.local/state/subspace")
        );
        assert_eq!(
            project_state_dir("tide").unwrap(),
            PathBuf::from("/home/me/.local/state/subspace/projects/tide")
        );
        assert_eq!(
            global_state_file().unwrap(),
            PathBuf::from("/home/me/.local/state/subspace/projects.json")
        );
    }

    #[test]
    fn an_unset_home_is_reported_only_when_needed() {
        assert!(matches!(
            resolve(None, None, ".config"),
            Err(Error::HomeNotSet)
        ));
        assert_eq!(
            resolve(Some("/xdg/config"), None, ".config").unwrap(),
            PathBuf::from("/xdg/config")
        );
    }
}
