use std::io::IsTerminal;

/// Verbosity of informational output from underlying commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Verbosity {
    /// Suppress informational output.
    Quiet,
    /// Default output level.
    #[default]
    Normal,
    /// Emit additional diagnostic output.
    Verbose,
}

/// Per-invocation settings for the calling client, resolved from the CLI's global flags and passed
/// into command handlers and the git layer. It carries client context only, not persisted settings.
#[derive(Debug, Clone, Copy)]
pub struct ClientSettings {
    /// Whether to emit ANSI color.
    pub color: bool,
    /// Whether the invocation can prompt for confirmation.
    pub interactive: bool,
    /// Output verbosity.
    pub verbosity: Verbosity,
}

impl ClientSettings {
    /// Resolves settings from client choices. A `None` for `color` or `interactive` means
    /// auto-detect: color against stdout, prompts against stdin.
    pub fn resolve(
        color: Option<bool>,
        interactive: Option<bool>,
        verbosity: Verbosity,
    ) -> ClientSettings {
        ClientSettings {
            color: resolve_color(color),
            interactive: interactive.unwrap_or_else(|| std::io::stdin().is_terminal()),
            verbosity,
        }
    }

    /// Returns a copy with `color` replaced, for a color choice that arrives after the global flags
    /// are resolved. `None` auto-detects, as [`ClientSettings::resolve`] does.
    pub fn with_color(self, color: Option<bool>) -> ClientSettings {
        ClientSettings {
            color: resolve_color(color),
            ..self
        }
    }

    /// Whether informational output from underlying commands is suppressed.
    pub fn quiet(&self) -> bool {
        matches!(self.verbosity, Verbosity::Quiet)
    }
}

/// Resolves a color choice. `None` means auto-detect against stdout.
fn resolve_color(color: Option<bool>) -> bool {
    color.unwrap_or_else(|| std::io::stdout().is_terminal())
}
