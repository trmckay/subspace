use serde::Deserialize;

use crate::{error::Error, hook::Hooks};

/// The `~/.config/subspace/config.json` document: what this user wants subspace to run before or
/// after a mutation.
///
/// Every field has a default, so an empty document behaves as one that carries every default. The
/// document is authored and subspace never writes it, so this type only deserializes. It
/// deserializes through [`RawGlobalSettings`], which validates the fields first, so a typo is
/// reported when the file is read rather than when a hook runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawGlobalSettings")]
pub struct GlobalSettings {
    /// Commands to run before or after a mutation, keyed by event. Each configured template is
    /// validated against
    /// [`expand_placeholders`](crate::hook::expand_placeholders)'s scanner when
    /// `~/.config/subspace/config.json` is read.
    hooks: Hooks,
}

/// The on-disk shape of [`GlobalSettings`]: the same fields with the same defaults, deserialized
/// first so they can be validated before they become the real type.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGlobalSettings {
    #[serde(default)]
    hooks: Hooks,
}

impl TryFrom<RawGlobalSettings> for GlobalSettings {
    type Error = Error;

    fn try_from(raw: RawGlobalSettings) -> Result<GlobalSettings, Error> {
        raw.hooks.validate()?;
        Ok(GlobalSettings { hooks: raw.hooks })
    }
}

impl GlobalSettings {
    /// Returns the commands to run before or after a mutation.
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{Event, Kind, Phase};

    #[test]
    fn an_absent_settings_object_takes_every_default() {
        let settings: GlobalSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, GlobalSettings::default());
        assert_eq!(settings.hooks(), &Hooks::default());
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = serde_json::from_str::<GlobalSettings>(r#"{"shell":"zsh"}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn an_unknown_event_inside_hooks_is_rejected() {
        let err = serde_json::from_str::<GlobalSettings>(r#"{"hooks":{"workspace_add":"x"}}"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown variant"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn config_apply_hooks_do_not_enter_subspace_hooks() {
        let settings: GlobalSettings = serde_json::from_str(
            r#"{"hooks":{"workspace_add_post":"create","config_apply_pre":"prepare","config_apply_post":"refresh"}}"#,
        )
        .unwrap();
        assert_eq!(
            settings
                .hooks()
                .command(Event::new(Kind::WorkspaceAdd, Phase::Post)),
            Some("create")
        );
        let workspace_only: GlobalSettings =
            serde_json::from_str(r#"{"hooks":{"workspace_add_post":"create"}}"#).unwrap();
        assert_eq!(settings.hooks(), workspace_only.hooks());
    }

    #[test]
    fn a_configured_hook_reads_back() {
        let settings: GlobalSettings =
            serde_json::from_str(r#"{"hooks":{"workspace_add_post":"init {workspace}"}}"#).unwrap();
        assert_eq!(
            settings
                .hooks()
                .command(Event::new(Kind::WorkspaceAdd, Phase::Post)),
            Some("init {workspace}")
        );
    }

    #[test]
    fn an_absent_hooks_object_leaves_every_hook_unset() {
        let settings: GlobalSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            settings
                .hooks()
                .command(Event::new(Kind::WorkspaceAdd, Phase::Post)),
            None
        );
    }

    #[test]
    fn an_unknown_placeholder_in_a_hook_is_rejected() {
        let err = serde_json::from_str::<GlobalSettings>(
            r#"{"hooks":{"workspace_add_pre":"run {wrokspace}"}}"#,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("workspace_add_pre") && message.contains("wrokspace"),
            "unclear error: {message}"
        );
    }

    #[test]
    fn dollar_brace_references_in_a_hook_are_not_placeholders() {
        let settings: GlobalSettings =
            serde_json::from_str(r#"{"hooks":{"workspace_add_post":"echo ${HOME}"}}"#).unwrap();
        assert_eq!(
            settings
                .hooks()
                .command(Event::new(Kind::WorkspaceAdd, Phase::Post)),
            Some("echo ${HOME}")
        );
    }
}
