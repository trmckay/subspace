use std::io::{self, Write};

use serde::Serialize;
use subspace::client::Verbosity;

/// The output router of one invocation. It owns three streams: reports that the command produces,
/// diagnostics for the human reader, and the machine document. Text mode writes reports to stdout.
/// JSON mode reserves stdout for one machine document, so it moves reports to the diagnostic
/// stream. Quiet and verbose mode govern the human streams alone.
pub struct Channel<R: Write, D: Write, M: Write> {
    report: R,
    diagnostic: D,
    machine: M,
    json: bool,
    verbosity: Verbosity,
    emitted: bool,
    failed: bool,
}

/// The channel of a real invocation. Reports and the machine document share stdout, because only
/// one of them can exist in one mode.
pub type Stdio = Channel<io::Stdout, io::Stderr, io::Stdout>;

/// The body of a machine error document. The code names the failure class and stays stable.
/// The details field carries a partial result, when the failure has one, and stays absent
/// otherwise.
#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

/// The one machine document of a failed invocation.
#[derive(Serialize)]
struct ErrorDocument<'a> {
    error: ErrorBody<'a>,
}

impl Stdio {
    /// The channel of a real invocation.
    pub fn stdio(json: bool, verbosity: Verbosity) -> Stdio {
        Channel::new(io::stdout(), io::stderr(), io::stdout(), json, verbosity)
    }
}

impl<R: Write, D: Write, M: Write> Channel<R, D, M> {
    /// A channel over explicit streams.
    pub fn new(report: R, diagnostic: D, machine: M, json: bool, verbosity: Verbosity) -> Self {
        Channel {
            report,
            diagnostic,
            machine,
            json,
            verbosity,
            emitted: false,
            failed: false,
        }
    }

    /// Whether the invocation writes a machine document.
    pub fn json(&self) -> bool {
        self.json
    }

    /// Consumes the channel and returns its machine stream. Callers that build a channel over a
    /// buffer, such as a test, use this to read what it wrote.
    #[cfg(test)]
    pub fn into_machine(self) -> M {
        self.machine
    }

    /// Writes one line of command output. Text mode uses stdout. JSON mode uses stderr.
    pub fn report(&mut self, line: &str) {
        if self.json {
            let _ = writeln!(self.diagnostic, "{line}");
        } else {
            let _ = writeln!(self.report, "{line}");
        }
    }

    /// Writes one informational line. Quiet mode drops it. It follows [`Channel::report`] for the
    /// stream.
    pub fn note(&mut self, line: &str) {
        if self.verbosity != Verbosity::Quiet {
            self.report(line);
        }
    }

    /// Writes one diagnostic line with no prefix. It always uses the diagnostic stream, in both
    /// modes, and quiet mode does not drop it.
    pub fn diagnostic(&mut self, line: &str) {
        let _ = writeln!(self.diagnostic, "{line}");
    }

    /// Writes one warning line. It always uses the diagnostic stream, in both modes, and quiet
    /// mode does not drop it.
    pub fn warn(&mut self, line: &str) {
        let _ = writeln!(self.diagnostic, "warning: {line}");
    }

    /// Writes the one success document of a JSON invocation. Text mode drops it, because the
    /// command already wrote its own report.
    pub fn success(&mut self, value: &impl Serialize) -> anyhow::Result<()> {
        if !self.json {
            return Ok(());
        }
        let text = serde_json::to_string(value)?;
        writeln!(self.machine, "{text}")?;
        self.emitted = true;
        Ok(())
    }

    /// Reports a failed invocation. JSON mode writes the one error document to stdout. Text mode
    /// keeps the `error:` line on stderr.
    pub fn failure(&mut self, code: &str, message: &str) {
        self.failure_with_details_value(code, message, None);
    }

    /// The shared body of [`Channel::failure`].
    fn failure_with_details_value(
        &mut self,
        code: &str,
        message: &str,
        details: Option<serde_json::Value>,
    ) {
        self.failed = true;
        if !self.json {
            let _ = writeln!(self.diagnostic, "error: {message}");
            return;
        }
        if self.emitted {
            let _ = writeln!(self.diagnostic, "error: {message}");
            return;
        }
        let document = ErrorDocument {
            error: ErrorBody {
                code,
                message,
                details,
            },
        };
        if let Ok(text) = serde_json::to_string(&document) {
            let _ = writeln!(self.machine, "{text}");
            self.emitted = true;
        }
    }

    /// Closes the invocation and returns its process status. A JSON invocation that wrote no
    /// document writes one error document, because stdout must carry exactly one.
    pub fn finish(&mut self) -> i32 {
        if self.json && !self.emitted {
            self.failure("no_result", "the command produced no machine result");
        }
        let _ = self.report.flush();
        let _ = self.diagnostic.flush();
        let _ = self.machine.flush();
        if self.failed { 1 } else { 0 }
    }
}

/// Whether automatic color detection uses stderr. JSON mode holds stdout for the machine document,
/// so the human streams decide the color.
pub fn auto_color_stream_is_stderr(json: bool) -> bool {
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel over three buffers, and the buffers themselves.
    struct Capture {
        channel: Channel<Vec<u8>, Vec<u8>, Vec<u8>>,
    }

    impl Capture {
        fn new(json: bool, verbosity: Verbosity) -> Capture {
            Capture {
                channel: Channel::new(Vec::new(), Vec::new(), Vec::new(), json, verbosity),
            }
        }

        fn report(&self) -> String {
            String::from_utf8(self.channel.report.clone()).unwrap()
        }

        fn diagnostic(&self) -> String {
            String::from_utf8(self.channel.diagnostic.clone()).unwrap()
        }

        fn machine(&self) -> String {
            String::from_utf8(self.channel.machine.clone()).unwrap()
        }
    }

    #[test]
    fn json_channel_text_mode_reports_on_stdout() {
        let mut c = Capture::new(false, Verbosity::Normal);
        c.channel.report("a");
        c.channel.note("b");
        assert_eq!(c.report(), "a\nb\n");
        assert_eq!(c.diagnostic(), "");
        assert_eq!(c.machine(), "");
    }

    #[test]
    fn json_channel_json_mode_moves_reports_to_stderr() {
        let mut c = Capture::new(true, Verbosity::Normal);
        c.channel.report("a");
        c.channel.note("b");
        assert_eq!(c.report(), "");
        assert_eq!(c.diagnostic(), "a\nb\n");
        assert_eq!(c.machine(), "");
    }

    #[test]
    fn json_channel_quiet_drops_notes_and_keeps_reports() {
        let mut c = Capture::new(true, Verbosity::Quiet);
        c.channel.report("a");
        c.channel.note("b");
        assert_eq!(c.diagnostic(), "a\n");
    }

    #[test]
    fn json_channel_verbose_keeps_notes() {
        let mut c = Capture::new(false, Verbosity::Verbose);
        c.channel.note("b");
        assert_eq!(c.report(), "b\n");
    }

    #[test]
    fn json_channel_diagnostics_stay_on_stderr_without_a_prefix() {
        for json in [false, true] {
            let mut c = Capture::new(json, Verbosity::Quiet);
            c.channel.diagnostic("d");
            assert_eq!(c.diagnostic(), "d\n");
            assert_eq!(c.report(), "");
            assert_eq!(c.machine(), "");
        }
    }

    #[test]
    fn json_channel_warnings_stay_on_stderr_in_both_modes() {
        for json in [false, true] {
            let mut c = Capture::new(json, Verbosity::Quiet);
            c.channel.warn("w");
            assert_eq!(c.diagnostic(), "warning: w\n");
            assert_eq!(c.report(), "");
            assert_eq!(c.machine(), "");
        }
    }

    #[test]
    fn json_channel_writes_one_success_document() {
        let mut c = Capture::new(true, Verbosity::Normal);
        c.channel
            .success(&serde_json::json!({"name": "w"}))
            .unwrap();
        assert_eq!(c.channel.finish(), 0);
        assert_eq!(c.machine(), "{\"name\":\"w\"}\n");
        assert_eq!(c.report(), "");
    }

    #[test]
    fn json_channel_verbosity_does_not_change_the_document() {
        let document = |verbosity| {
            let mut c = Capture::new(true, verbosity);
            c.channel.note("b");
            c.channel.warn("w");
            c.channel
                .success(&serde_json::json!({"name": "w"}))
                .unwrap();
            c.machine()
        };
        assert_eq!(document(Verbosity::Quiet), document(Verbosity::Normal));
        assert_eq!(document(Verbosity::Verbose), document(Verbosity::Normal));
    }

    #[test]
    fn json_channel_text_mode_drops_the_success_document() {
        let mut c = Capture::new(false, Verbosity::Normal);
        c.channel
            .success(&serde_json::json!({"name": "w"}))
            .unwrap();
        assert_eq!(c.machine(), "");
        assert_eq!(c.channel.finish(), 0);
    }

    #[test]
    fn json_channel_error_document_goes_to_stdout_with_a_failed_status() {
        let mut c = Capture::new(true, Verbosity::Normal);
        c.channel.failure("unsupported", "no JSON result");
        assert_eq!(c.channel.finish(), 1);
        let value: serde_json::Value = serde_json::from_str(c.machine().trim()).unwrap();
        assert_eq!(value["error"]["code"], "unsupported");
        assert_eq!(value["error"]["message"], "no JSON result");
        assert_eq!(c.report(), "");
        assert_eq!(c.diagnostic(), "");
    }

    #[test]
    fn json_channel_text_mode_keeps_the_error_line_on_stderr() {
        let mut c = Capture::new(false, Verbosity::Normal);
        c.channel.failure("failed", "boom");
        assert_eq!(c.channel.finish(), 1);
        assert_eq!(c.diagnostic(), "error: boom\n");
        assert_eq!(c.machine(), "");
    }

    #[test]
    fn json_channel_keeps_one_document_after_a_late_failure() {
        let mut c = Capture::new(true, Verbosity::Normal);
        c.channel
            .success(&serde_json::json!({"name": "w"}))
            .unwrap();
        c.channel.failure("failed", "boom");
        assert_eq!(c.channel.finish(), 1);
        assert_eq!(c.machine(), "{\"name\":\"w\"}\n");
        assert_eq!(c.diagnostic(), "error: boom\n");
    }

    #[test]
    fn json_channel_finish_writes_a_document_when_a_command_wrote_none() {
        let mut c = Capture::new(true, Verbosity::Normal);
        assert_eq!(c.channel.finish(), 1);
        let value: serde_json::Value = serde_json::from_str(c.machine().trim()).unwrap();
        assert_eq!(value["error"]["code"], "no_result");
    }

    #[test]
    fn json_channel_error_document_omits_details_when_absent() {
        let mut c = Capture::new(true, Verbosity::Normal);
        c.channel.failure("failed", "boom");
        assert_eq!(c.channel.finish(), 1);
        let value: serde_json::Value = serde_json::from_str(c.machine().trim()).unwrap();
        assert!(!value["error"].as_object().unwrap().contains_key("details"));
    }

    #[test]
    fn json_channel_auto_color_reads_stderr_only_in_json_mode() {
        assert!(auto_color_stream_is_stderr(true));
        assert!(!auto_color_stream_is_stderr(false));
    }
}
