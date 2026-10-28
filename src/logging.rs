use std::error::Error;

use crate::ui::ConsoleUi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSeverity {
    Info,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticCategory {
    General,
    Queue,
    Thumbnail,
    Scraper,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub severity: LogSeverity,
    pub category: DiagnosticCategory,
    pub message: String,
}

pub trait DiagnosticSink {
    fn emit(&self, event: &DiagnosticEvent);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StderrDiagnosticSink;

impl DiagnosticSink for StderrDiagnosticSink {
    fn emit(&self, event: &DiagnosticEvent) {
        eprintln!("{}", format_diagnostic_event(event));
    }
}

pub struct UiDiagnosticSink<'a> {
    ui: &'a ConsoleUi,
}

impl<'a> UiDiagnosticSink<'a> {
    pub fn new(ui: &'a ConsoleUi) -> Self {
        Self { ui }
    }
}

impl DiagnosticSink for UiDiagnosticSink<'_> {
    fn emit(&self, event: &DiagnosticEvent) {
        match event.severity {
            LogSeverity::Info => self.ui.print_status(&event.message),
            LogSeverity::Warn => self.ui.print_warning(&format_diagnostic_event(event)),
        }
    }
}

pub fn emit_diagnostic(event: &DiagnosticEvent, sinks: &[&dyn DiagnosticSink]) {
    for sink in sinks {
        sink.emit(event);
    }
}

pub fn format_log_message(severity: LogSeverity, message: &str) -> String {
    let label = match severity {
        LogSeverity::Info => "info",
        LogSeverity::Warn => "warn",
    };
    format!("[{}] {}", label, message)
}

pub fn format_diagnostic_event(event: &DiagnosticEvent) -> String {
    format_log_message(event.severity, &event.message)
}

pub fn diagnostic_message_event(
    severity: LogSeverity,
    category: DiagnosticCategory,
    message: impl Into<String>,
) -> DiagnosticEvent {
    DiagnosticEvent {
        severity,
        category,
        message: message.into(),
    }
}

pub fn non_fatal_error_event(
    category: DiagnosticCategory,
    context: &str,
    error: &dyn Error,
) -> DiagnosticEvent {
    diagnostic_message_event(
        LogSeverity::Warn,
        category,
        format!("{}: {}", context, error),
    )
}

pub fn format_non_fatal_error(context: &str, error: &dyn Error) -> String {
    format_diagnostic_event(&non_fatal_error_event(
        DiagnosticCategory::General,
        context,
        error,
    ))
}

pub fn log_non_fatal_error(category: DiagnosticCategory, context: &str, error: &dyn Error) {
    let event = non_fatal_error_event(category, context, error);
    emit_diagnostic(&event, &[&StderrDiagnosticSink]);
}

pub fn log_diagnostic_message(category: DiagnosticCategory, message: &str) {
    let event = diagnostic_message_event(LogSeverity::Info, category, message);
    emit_diagnostic(&event, &[&StderrDiagnosticSink]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StubError(&'static str);

    impl std::fmt::Display for StubError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for StubError {}

    #[test]
    fn format_log_message_prefixes_info_messages() {
        assert_eq!(
            format_log_message(LogSeverity::Info, "queue resumed"),
            "[info] queue resumed"
        );
    }

    #[test]
    fn format_log_message_prefixes_warning_messages() {
        assert_eq!(
            format_log_message(LogSeverity::Warn, "thumbnail generation failed"),
            "[warn] thumbnail generation failed"
        );
    }

    #[test]
    fn format_non_fatal_error_includes_context_and_error_message() {
        let error = StubError("disk full");

        let formatted = format_non_fatal_error("persist pending queue", &error);

        assert_eq!(formatted, "[warn] persist pending queue: disk full");
    }

    #[test]
    fn diagnostic_event_stores_severity_category_and_message() {
        let event = diagnostic_message_event(
            LogSeverity::Info,
            DiagnosticCategory::Queue,
            "queue resumed",
        );

        assert_eq!(event.severity, LogSeverity::Info);
        assert_eq!(event.category, DiagnosticCategory::Queue);
        assert_eq!(event.message, "queue resumed");
    }

    #[test]
    fn format_diagnostic_event_uses_event_message_and_severity() {
        let event = diagnostic_message_event(
            LogSeverity::Warn,
            DiagnosticCategory::Thumbnail,
            "thumbnail generation failed",
        );

        assert_eq!(
            format_diagnostic_event(&event),
            "[warn] thumbnail generation failed"
        );
    }

    #[test]
    fn ui_diagnostic_sink_routes_info_and_warning_messages() {
        let log = crate::ui::UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());
        let sink = UiDiagnosticSink::new(&ui);
        let info = diagnostic_message_event(
            LogSeverity::Info,
            DiagnosticCategory::Queue,
            "queue resumed",
        );
        let warn = diagnostic_message_event(
            LogSeverity::Warn,
            DiagnosticCategory::Thumbnail,
            "thumbnail generation failed",
        );

        emit_diagnostic(&info, &[&sink]);
        emit_diagnostic(&warn, &[&sink]);

        assert_eq!(
            log.snapshot(),
            vec!["queue resumed", "[warn] thumbnail generation failed"]
        );
    }
}
