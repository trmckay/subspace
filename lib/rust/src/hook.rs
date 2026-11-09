//! Commands the user configures subspace to run around a mutation.
//!
//! Subspace changes worktrees, metadata, and snapshots. Everything else that a
//! mutation should trigger, such as a terminal session, belongs to another tool. A hook is the
//! seam: one command template per event, configured in `~/.config/subspace/config.json`.
//!
//! Each mutation carries two events, a `_pre` event before the change and a `_post` event after
//! it. A `_pre` command that fails aborts the command before anything changes. A `_post` command
//! that fails fails the command after the change lands.

use std::{collections::BTreeMap, process::Command};

use serde::{
    Deserialize,
    de::{self, Deserializer, Visitor},
};

use crate::error::Error;

/// A mutation subspace reports to a configured hook command.
///
/// This is the whole kind set. Each kind carries both phases, so the config holds 18 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A workspace is created.
    WorkspaceAdd,
    /// A workspace is removed.
    WorkspaceRemove,
    /// A workspace is renamed.
    WorkspaceRename,
    /// A workspace gets a new parent edge.
    WorkspaceReparent,
    /// A workspace worktree link is repaired.
    WorkspaceRelink,
    /// A project is created.
    ProjectAdd,
    /// A project is deregistered.
    ProjectRemove,
    /// A project is renamed.
    ProjectRename,
    /// A project directory moves.
    ProjectMove,
}

impl Kind {
    /// Returns the configured name of the kind, which is the event name without its phase.
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::WorkspaceAdd => "workspace_add",
            Kind::WorkspaceRemove => "workspace_remove",
            Kind::WorkspaceRename => "workspace_rename",
            Kind::WorkspaceReparent => "workspace_reparent",
            Kind::WorkspaceRelink => "workspace_relink",
            Kind::ProjectAdd => "project_add",
            Kind::ProjectRemove => "project_remove",
            Kind::ProjectRename => "project_rename",
            Kind::ProjectMove => "project_move",
        }
    }

    /// Returns the kind that `name` spells, or `None` when no kind carries that name.
    fn parse(name: &str) -> Option<Kind> {
        ALL_KINDS.into_iter().find(|kind| kind.as_str() == name)
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// When a hook runs against the mutation it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Before the change, and before the project lock. A failure aborts the command.
    Pre,
    /// After the change, and after the project lock drops. A failure fails the command and the
    /// change stays.
    Post,
}

impl Phase {
    /// Returns the configured suffix of the phase.
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Pre => "pre",
            Phase::Post => "post",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One configured hook name: a kind and a phase, spelled `<kind>_<phase>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Event {
    kind: Kind,
    phase: Phase,
}

impl Event {
    /// Creates the event of `kind` in `phase`.
    pub fn new(kind: Kind, phase: Phase) -> Event {
        Event { kind, phase }
    }

    /// Returns the event that `name` spells, or `None` when no event carries that name.
    fn parse(name: &str) -> Option<Event> {
        for phase in [Phase::Pre, Phase::Post] {
            let suffix = format!("_{phase}");
            if let Some(kind) = name.strip_suffix(&suffix).and_then(Kind::parse) {
                return Some(Event::new(kind, phase));
            }
        }
        None
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}_{}", self.kind, self.phase)
    }
}

/// Every kind, in the order the events are documented.
const ALL_KINDS: [Kind; 9] = [
    Kind::WorkspaceAdd,
    Kind::WorkspaceRemove,
    Kind::WorkspaceRename,
    Kind::WorkspaceReparent,
    Kind::WorkspaceRelink,
    Kind::ProjectAdd,
    Kind::ProjectRemove,
    Kind::ProjectRename,
    Kind::ProjectMove,
];

/// Every configured event name, which a rejected key lists as the names it accepts.
const EVENT_NAMES: [&str; 18] = [
    "workspace_add_pre",
    "workspace_add_post",
    "workspace_remove_pre",
    "workspace_remove_post",
    "workspace_rename_pre",
    "workspace_rename_post",
    "workspace_reparent_pre",
    "workspace_reparent_post",
    "workspace_relink_pre",
    "workspace_relink_post",
    "project_add_pre",
    "project_add_post",
    "project_remove_pre",
    "project_remove_post",
    "project_rename_pre",
    "project_rename_post",
    "project_move_pre",
    "project_move_post",
];

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Event, D::Error> {
        struct EventVisitor;

        impl Visitor<'_> for EventVisitor {
            type Value = Event;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a hook event name")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Event, E> {
                Event::parse(value).ok_or_else(|| de::Error::unknown_variant(value, &EVENT_NAMES))
            }
        }

        deserializer.deserialize_str(EventVisitor)
    }
}

/// A source of placeholder values for [`expand_placeholders`].
pub trait Values {
    /// Returns the value for `name`, or `None` when the source defines no such placeholder.
    fn get(&self, name: &str) -> Option<&str>;
}

/// The values a hook command's placeholders expand to.
///
/// Every field is a string, and a field an event does not carry is empty. So a template always
/// expands, and a command that reads an empty argument decides what to do about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    project: String,
    workspace: String,
    path: String,
    parent: String,
    previous: String,
    previous_path: String,
    env: Vec<(String, String)>,
}

impl Fields {
    /// Creates the fields for an event in `project` at `path`.
    pub fn new(project: impl Into<String>, path: impl Into<String>) -> Fields {
        Fields {
            project: project.into(),
            path: path.into(),
            ..Fields::default()
        }
    }

    /// Returns the same fields with `env` as the environment the hook command runs with, on top of
    /// the process environment. A workspace event's env carries PROJECT_NAME, PROJECT_PATH,
    /// PROJECT_CONFIG, and WORKSPACE_NAME.
    pub fn env(mut self, env: Vec<(String, String)>) -> Fields {
        self.env = env;
        self
    }

    /// Returns the same fields with `workspace` as the workspace name.
    pub fn workspace(mut self, workspace: impl Into<String>) -> Fields {
        self.workspace = workspace.into();
        self
    }

    /// Returns the same fields with `parent` as the parent workspace name.
    pub fn parent(mut self, parent: impl Into<String>) -> Fields {
        self.parent = parent.into();
        self
    }

    /// Returns the same fields with `previous` as the name the subject carried before a rename, or
    /// the parent it had before a reparent.
    pub fn previous(mut self, previous: impl Into<String>) -> Fields {
        self.previous = previous.into();
        self
    }

    /// Returns the same fields with `previous_path` as the path the subject sat at before.
    pub fn previous_path(mut self, previous_path: impl Into<String>) -> Fields {
        self.previous_path = previous_path.into();
        self
    }
}

impl Values for Fields {
    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "project" => Some(&self.project),
            "workspace" => Some(&self.workspace),
            "path" => Some(&self.path),
            "parent" => Some(&self.parent),
            "previous" => Some(&self.previous),
            "previous_path" => Some(&self.previous_path),
            _ => None,
        }
    }
}

/// Subspace hook commands keyed by event. Config apply hook commands in the shared map are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hooks {
    commands: BTreeMap<Event, String>,
}

impl<'de> Deserialize<'de> for Hooks {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Hooks, D::Error> {
        let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
        let mut commands = BTreeMap::new();
        for (name, command) in raw {
            if matches!(name.as_str(), "config_apply_pre" | "config_apply_post") {
                continue;
            }
            let event = Event::parse(&name)
                .ok_or_else(|| de::Error::unknown_variant(&name, &EVENT_NAMES))?;
            commands.insert(event, command);
        }
        Ok(Hooks { commands })
    }
}

impl Hooks {
    /// Returns the command template configured for `event`, if any.
    pub fn command(&self, event: Event) -> Option<&str> {
        self.commands.get(&event).map(String::as_str)
    }

    /// Validates every configured template against the placeholder scanner, so a malformed or
    /// unknown placeholder is reported when `~/.config/subspace/config.json` is read.
    pub fn validate(&self) -> Result<(), Error> {
        for (event, template) in &self.commands {
            expand_placeholders(template, &Fields::default())
                .map_err(|e| Error::Setting(format!("hooks.{event}: {e}")))?;
        }
        Ok(())
    }
}

/// How a runner runs one expanded command, with the environment to run it under. It returns a
/// message when the command did not succeed.
pub type HookExec = Box<dyn Fn(&str, &[(String, String)]) -> Result<(), String>>;

/// Which stream of the caller a hook command writes its stdout to.
///
/// A hook writes for the human reader, so its output belongs on the human stream. Text mode keeps
/// stdout, because stdout is that stream. JSON mode holds stdout for the one machine document, so
/// it moves the hook to stderr. Without the move, a hook that prints anything corrupts the
/// document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdout {
    /// The stdout of the caller, which text mode uses.
    Inherit,
    /// The stderr of the caller, which JSON mode uses.
    Diagnostic,
}

/// Runs the configured command for an event.
///
/// An event with no configured command runs nothing and succeeds. A configured command that fails
/// to expand, fails to spawn, or exits non-zero returns [`Error::HookFailed`], so a `_pre` command
/// aborts the mutation and a `_post` command fails the finished command. The runner holds an
/// injectable [`HookExec`], which tests replace to record commands instead of running them.
pub struct HookRunner {
    hooks: Hooks,
    exec: HookExec,
}

impl HookRunner {
    /// Creates a runner that runs each command under `sh -c`, with the streams of the caller, and
    /// sends its stdout to `stdout`.
    ///
    /// The shell is fixed. A hook is one line of shell glue that the user authors, so it needs a
    /// shell every machine has, not the interactive shell of the user.
    pub fn new(hooks: Hooks, stdout: Stdout) -> HookRunner {
        HookRunner::with_exec(
            hooks,
            Box::new(move |command, env| shell_exec(command, env, stdout)),
        )
    }

    /// Creates a runner that runs each command through `exec`.
    pub fn with_exec(hooks: Hooks, exec: HookExec) -> HookRunner {
        HookRunner { hooks, exec }
    }

    /// Runs the command configured for `kind` in `phase`, with `fields` expanded into it.
    pub fn run(&self, kind: Kind, phase: Phase, fields: &Fields) -> Result<(), Error> {
        let event = Event::new(kind, phase);
        let Some(template) = self.hooks.command(event) else {
            return Ok(());
        };
        let failed = |message: String| Error::HookFailed {
            event: event.to_string(),
            message,
        };
        let command = expand_placeholders(template, fields).map_err(|e| failed(e.to_string()))?;
        (self.exec)(&command, &fields.env).map_err(failed)
    }
}

/// Runs every `_post` hook in `pending` against `hooks`, even after one fails.
///
/// A `_post` hook runs after its change has already landed, so a mutation with more than one
/// target, such as a recursive `remove`, must not let an earlier failure stop a later target's
/// hook from running. This runs each pair in order, collects every failure, and returns one
/// [`Error::HooksFailed`] naming them all, rather than [`Error::HookFailed`] for only the first.
pub fn run_post_hooks(hooks: &HookRunner, pending: &[(Kind, Fields)]) -> Result<(), Error> {
    let failures: Vec<String> = pending
        .iter()
        .filter_map(|(kind, fields)| hooks.run(*kind, Phase::Post, fields).err())
        .map(|error| error.to_string())
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::HooksFailed { events: failures })
    }
}

/// Runs `command` under `sh -c`, with `env` layered onto the process environment, and lets it write
/// to the streams of the caller, so its output reaches the human channel.
///
/// Under [`Stdout::Diagnostic`], the shell redirects its own stdout to stderr before it runs the
/// command, so every descendant writes there too. The redirect is a shell line rather than a file
/// descriptor call, so it holds on every platform that has `sh`. The error message names the
/// command the user configured, not the redirected script.
fn shell_exec(command: &str, env: &[(String, String)], stdout: Stdout) -> Result<(), String> {
    match Command::new("sh")
        .args(["-c", &shell_script(command, stdout)])
        .envs(env.iter().cloned())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{command}: exit {}", status.code().unwrap_or(-1))),
        Err(error) => Err(format!("{command}: {error}")),
    }
}

/// The shell script that runs `command` with its stdout on the stream that `stdout` names.
///
/// The `exec` line moves the stdout of the shell itself, so the command and everything it starts
/// inherit the moved stream. It is a separate line, so a command that starts with a comment or a
/// redirect still parses.
fn shell_script(command: &str, stdout: Stdout) -> String {
    match stdout {
        Stdout::Inherit => command.to_string(),
        Stdout::Diagnostic => format!("exec 1>&2\n{command}"),
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(|c| {
        c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '=' | ',' | '+')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Expands `{...}` placeholders in `template` against `values`.
///
/// Each value becomes one shell-quoted word. A `{` immediately preceded by `$` is never a
/// placeholder. `${...}` always stays literal text, so a shell parameter expansion like `${path}`
/// passes through untouched, even though `path` is a real placeholder name. Otherwise a `{...}`
/// counts as a placeholder attempt only when its contents form a placeholder-shaped name
/// (lowercase ASCII letters and underscores, non-empty) immediately followed by `}`. An unknown
/// name is an error, so a typo in configuration fails instead of silently doing nothing. Anything
/// else, such as an `awk '{print $1}'` program or an unterminated `{`, is copied through as literal
/// text, because a hook is one line of shell glue that can carry either.
pub fn expand_placeholders(template: &str, values: &dyn Values) -> Result<String, Error> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        if open > 0 && rest.as_bytes()[open - 1] == b'$' {
            result.push('{');
            rest = &rest[open + 1..];
            continue;
        }
        let tail = &rest[open + 1..];
        let name_len = tail
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..name_len];
        if !name.is_empty() && tail[name_len..].starts_with('}') {
            let value = values
                .get(name)
                .ok_or_else(|| Error::Placeholder(format!("unknown placeholder {{{name}}}")))?;
            result.push_str(&shell_quote(value));
            rest = &tail[name_len + 1..];
        } else {
            result.push('{');
            rest = tail;
        }
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    fn fields() -> Fields {
        Fields::new("config", "/w/terminal")
            .workspace("terminal")
            .parent("main")
            .previous("old")
            .previous_path("/old/config")
    }

    /// Builds a runner that records each command it is given instead of running it.
    fn recording(hooks: Hooks) -> (HookRunner, Rc<RefCell<Vec<String>>>) {
        let (runner, log, _envs) = recording_with_env(hooks);
        (runner, log)
    }

    /// Builds a runner that records each command and the environment it was given, instead of
    /// running it.
    fn recording_with_env(
        hooks: Hooks,
    ) -> (
        HookRunner,
        Rc<RefCell<Vec<String>>>,
        Rc<RefCell<Vec<Vec<(String, String)>>>>,
    ) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let envs = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&log);
        let env_sink = Rc::clone(&envs);
        let runner = HookRunner::with_exec(
            hooks,
            Box::new(move |command: &str, env: &[(String, String)]| {
                sink.borrow_mut().push(command.to_string());
                env_sink.borrow_mut().push(env.to_vec());
                Ok(())
            }),
        );
        (runner, log, envs)
    }

    /// Builds a runner whose commands always fail.
    fn failing(hooks: Hooks) -> (HookRunner, Rc<RefCell<Vec<String>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&log);
        let runner = HookRunner::with_exec(
            hooks,
            Box::new(move |command: &str, _env: &[(String, String)]| {
                sink.borrow_mut().push(command.to_string());
                Err("exit 1".to_string())
            }),
        );
        (runner, log)
    }

    fn parse(json: &str) -> Hooks {
        serde_json::from_str(json).unwrap()
    }

    /// Every event, built from the kind list and both phases.
    fn all_events() -> Vec<Event> {
        ALL_KINDS
            .into_iter()
            .flat_map(|kind| [Event::new(kind, Phase::Pre), Event::new(kind, Phase::Post)])
            .collect()
    }

    #[test]
    fn hook_names_every_event_once() {
        let mut names: Vec<String> = all_events().iter().map(Event::to_string).collect();
        names.sort();
        let mut expected: Vec<String> = EVENT_NAMES.iter().map(|n| n.to_string()).collect();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn hook_event_names_round_trip_through_serde() {
        for event in all_events() {
            let json = format!("\"{event}\"");
            let parsed: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn hook_expands_every_placeholder() {
        let expanded = expand_placeholders(
            "run {project} {workspace} {path} {parent} {previous} {previous_path}",
            &fields(),
        )
        .unwrap();
        assert_eq!(
            expanded,
            "run config terminal /w/terminal main old /old/config"
        );
    }

    #[test]
    fn hook_expands_a_template_without_placeholders() {
        assert_eq!(expand_placeholders("plain", &fields()).unwrap(), "plain");
    }

    #[test]
    fn hook_expands_repeated_and_adjacent_placeholders() {
        let expanded = expand_placeholders("{project}{project}/{workspace}", &fields()).unwrap();
        assert_eq!(expanded, "configconfig/terminal");
    }

    #[test]
    fn hook_expands_an_absent_field_to_nothing() {
        let fields = Fields::new("config", "/p");
        assert_eq!(
            expand_placeholders("[{workspace}]", &fields).unwrap(),
            "['']"
        );
    }

    #[test]
    fn hook_shell_quotes_unsafe_placeholder_text() {
        let fields = Fields::new("config", "/tmp/a'b; touch injected");
        assert_eq!(
            expand_placeholders("run {path}", &fields).unwrap(),
            "run '/tmp/a'\\''b; touch injected'"
        );
    }

    #[test]
    fn hook_rejects_an_unknown_placeholder() {
        assert!(matches!(
            expand_placeholders("{porject}", &fields()),
            Err(Error::Placeholder(_))
        ));
    }

    #[test]
    fn hook_keeps_an_unterminated_placeholder_verbatim() {
        assert_eq!(
            expand_placeholders("{project", &fields()).unwrap(),
            "{project"
        );
    }

    #[test]
    fn hook_keeps_a_dollar_brace_reference_verbatim() {
        assert_eq!(
            expand_placeholders("echo ${HOME}", &fields()).unwrap(),
            "echo ${HOME}"
        );
        assert_eq!(
            expand_placeholders("echo ${path}", &fields()).unwrap(),
            "echo ${path}"
        );
        assert_eq!(
            expand_placeholders("echo ${foo}", &fields()).unwrap(),
            "echo ${foo}"
        );
    }

    #[test]
    fn hook_keeps_a_dollar_before_other_text_verbatim() {
        assert_eq!(
            expand_placeholders("cost is $5 for {project}", &fields()).unwrap(),
            "cost is $5 for config"
        );
    }

    #[test]
    fn hook_keeps_an_awk_program_verbatim() {
        assert_eq!(
            expand_placeholders("awk '{print $1}'", &fields()).unwrap(),
            "awk '{print $1}'"
        );
    }

    #[test]
    fn hook_reads_every_event_name() {
        let entries: Vec<String> = EVENT_NAMES
            .iter()
            .map(|name| format!("\"{name}\": \"run {name}\""))
            .collect();
        let hooks = parse(&format!("{{{}}}", entries.join(",")));
        for event in all_events() {
            assert_eq!(hooks.command(event), Some(format!("run {event}").as_str()));
        }
    }

    #[test]
    fn hook_rejects_an_unknown_event_name() {
        let err = serde_json::from_str::<Hooks>(r#"{"workspace_create_post":"x"}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown variant"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn hook_rejects_an_event_name_without_a_phase() {
        let err = serde_json::from_str::<Hooks>(r#"{"workspace_add":"x"}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown variant"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn hook_leaves_an_unconfigured_event_unset() {
        let hooks = parse(r#"{"workspace_add_post":"a"}"#);
        assert_eq!(
            hooks.command(Event::new(Kind::WorkspaceAdd, Phase::Pre)),
            None
        );
        assert_eq!(
            hooks.command(Event::new(Kind::WorkspaceRemove, Phase::Post)),
            None
        );
        assert_eq!(
            Hooks::default().command(Event::new(Kind::WorkspaceAdd, Phase::Post)),
            None
        );
    }

    #[test]
    fn hook_validate_rejects_an_unknown_placeholder() {
        let hooks = parse(r#"{"workspace_add_post":"run {porject}"}"#);
        let err = hooks.validate().unwrap_err().to_string();
        assert!(
            err.contains("workspace_add_post") && err.contains("porject"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn hook_validate_accepts_every_known_placeholder() {
        let hooks = parse(
            r#"{"workspace_add_pre":"run {project} {workspace} {path} {parent} {previous} {previous_path}"}"#,
        );
        assert!(hooks.validate().is_ok());
    }

    #[test]
    fn hook_runs_the_command_of_its_event_expanded() {
        let (runner, log) = recording(parse(
            r#"{"workspace_add_post":"add {project}/{workspace}","workspace_remove_post":"rm {workspace}"}"#,
        ));
        runner
            .run(Kind::WorkspaceAdd, Phase::Post, &fields())
            .unwrap();
        assert_eq!(log.borrow().as_slice(), ["add config/terminal"]);
    }

    #[test]
    fn hook_runs_each_phase_of_a_kind_separately() {
        let (runner, log) = recording(parse(
            r#"{"workspace_add_pre":"before {workspace}","workspace_add_post":"after {workspace}"}"#,
        ));
        runner
            .run(Kind::WorkspaceAdd, Phase::Pre, &fields())
            .unwrap();
        runner
            .run(Kind::WorkspaceAdd, Phase::Post, &fields())
            .unwrap();
        assert_eq!(
            log.borrow().as_slice(),
            ["before terminal", "after terminal"]
        );
    }

    #[test]
    fn hook_runs_nothing_for_an_unconfigured_event() {
        let (runner, log) = recording(parse(r#"{"workspace_add_post":"add"}"#));
        runner
            .run(Kind::WorkspaceRemove, Phase::Post, &fields())
            .unwrap();
        runner
            .run(Kind::WorkspaceAdd, Phase::Pre, &fields())
            .unwrap();
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn hook_runs_the_command_with_its_env() {
        let (runner, _log, envs) =
            recording_with_env(parse(r#"{"workspace_add_post":"add {workspace}"}"#));
        let fields = fields().env(vec![("PROJECT_NAME".to_string(), "config".to_string())]);
        runner
            .run(Kind::WorkspaceAdd, Phase::Post, &fields)
            .unwrap();
        assert_eq!(
            envs.borrow().as_slice(),
            [[("PROJECT_NAME".to_string(), "config".to_string())]]
        );
    }

    #[test]
    fn hook_fails_the_command_when_a_pre_command_fails() {
        let (runner, log) = failing(parse(r#"{"workspace_add_pre":"false"}"#));
        let err = runner
            .run(Kind::WorkspaceAdd, Phase::Pre, &fields())
            .unwrap_err();
        assert_eq!(log.borrow().as_slice(), ["false"]);
        assert!(
            matches!(&err, Error::HookFailed { event, .. } if event == "workspace_add_pre"),
            "unclear error: {err}"
        );
        assert!(
            err.to_string().contains("workspace_add_pre"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn hook_fails_the_command_when_a_post_command_fails() {
        let (runner, log) = failing(parse(r#"{"workspace_remove_post":"false"}"#));
        let err = runner
            .run(Kind::WorkspaceRemove, Phase::Post, &fields())
            .unwrap_err();
        assert_eq!(log.borrow().as_slice(), ["false"]);
        assert!(
            matches!(&err, Error::HookFailed { event, .. } if event == "workspace_remove_post"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn run_post_hooks_runs_every_pending_hook_even_after_one_fails() {
        let (runner, log) = failing(parse(
            r#"{"workspace_remove_post":"rm {workspace}","workspace_reparent_post":"mv {workspace}"}"#,
        ));
        let pending = vec![
            (Kind::WorkspaceRemove, fields().workspace("a")),
            (Kind::WorkspaceReparent, fields().workspace("b")),
        ];
        let err = run_post_hooks(&runner, &pending).unwrap_err();
        assert_eq!(log.borrow().as_slice(), ["rm a", "mv b"]);
        assert!(
            matches!(&err, Error::HooksFailed { events } if events.len() == 2),
            "unclear error: {err}"
        );
    }

    #[test]
    fn run_post_hooks_names_every_failure_in_the_error() {
        let (runner, _log) = failing(parse(
            r#"{"workspace_remove_post":"rm {workspace}","workspace_reparent_post":"mv {workspace}"}"#,
        ));
        let pending = vec![
            (Kind::WorkspaceRemove, fields().workspace("a")),
            (Kind::WorkspaceReparent, fields().workspace("b")),
        ];
        let err = run_post_hooks(&runner, &pending).unwrap_err().to_string();
        assert!(
            err.contains("workspace_remove_post") && err.contains("workspace_reparent_post"),
            "unclear error: {err}"
        );
    }

    #[test]
    fn run_post_hooks_succeeds_when_every_hook_succeeds() {
        let (runner, log) = recording(parse(
            r#"{"workspace_remove_post":"rm {workspace}","workspace_reparent_post":"mv {workspace}"}"#,
        ));
        let pending = vec![
            (Kind::WorkspaceRemove, fields().workspace("a")),
            (Kind::WorkspaceReparent, fields().workspace("b")),
        ];
        run_post_hooks(&runner, &pending).unwrap();
        assert_eq!(log.borrow().as_slice(), ["rm a", "mv b"]);
    }

    #[test]
    fn shell_script_keeps_stdout_in_text_mode() {
        assert_eq!(shell_script("echo hi", Stdout::Inherit), "echo hi");
    }

    #[test]
    fn shell_script_moves_stdout_to_stderr_in_json_mode() {
        assert_eq!(
            shell_script("echo hi", Stdout::Diagnostic),
            "exec 1>&2\necho hi"
        );
    }

    #[test]
    fn shell_script_puts_the_redirect_on_its_own_line() {
        let script = shell_script("# a comment", Stdout::Diagnostic);
        assert_eq!(script.lines().next(), Some("exec 1>&2"));
        assert_eq!(script.lines().nth(1), Some("# a comment"));
    }

    #[test]
    fn shell_exec_reports_the_status_of_a_command_in_both_modes() {
        for stdout in [Stdout::Inherit, Stdout::Diagnostic] {
            shell_exec("true", &[], stdout).unwrap();
            let err = shell_exec("exit 3", &[], stdout).unwrap_err();
            assert_eq!(err, "exit 3: exit 3");
        }
    }

    #[test]
    fn shell_exec_runs_a_command_that_writes_to_stdout_in_json_mode() {
        shell_exec("echo hi", &[], Stdout::Diagnostic).unwrap();
    }

    #[test]
    fn shell_exec_passes_the_environment_in_both_modes() {
        for stdout in [Stdout::Inherit, Stdout::Diagnostic] {
            let env = vec![("HOOK_TEST".to_string(), "1".to_string())];
            shell_exec("test \"$HOOK_TEST\" = 1", &env, stdout).unwrap();
        }
    }
}
