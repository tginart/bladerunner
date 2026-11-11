//! Stderr writes for paths where fd 2 can already be dead.
//!
//! When the reader of stderr is gone (a closed terminal pane, or a pipe whose
//! reader exited), each write fails and `eprintln!` panics. The release profile
//! sets `panic = "abort"`, so that panic kills the process. The exit error
//! report, the slow session-end notice, and the headless plain-format
//! diagnostics run when stderr is often exactly such a dead terminal. These
//! paths write through this module: one attempt, and a failure is returned,
//! not raised.

use std::io::Write;

/// Write `line` and a newline to `writer`. Return false if the write failed.
///
/// The writer is a parameter so that a test can use a closed pipe.
pub fn write_line(writer: &mut impl Write, line: &str) -> bool {
    writeln!(writer, "{line}").is_ok()
}

/// [`write_line`] to the process stderr.
///
/// The result is discarded on purpose: there is no other channel to report
/// a dead stderr on, and the caller is about to exit or carry on.
pub fn eprint_line(line: &str) {
    write_line(&mut std::io::stderr(), line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_line_appends_a_newline_and_reports_success() {
        let mut buf = Vec::new();
        assert!(write_line(&mut buf, "Finishing session…"));
        assert_eq!(buf, "Finishing session…\n".as_bytes());
    }

    /// A closed read end makes each write fail with EPIPE, as the dead tty of
    /// a closed pane does. Rust binaries ignore SIGPIPE, so the write returns
    /// an error instead of a signal.
    #[test]
    fn write_line_reports_failure_on_a_dead_pipe() {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        drop(reader);
        assert!(!write_line(&mut writer, "unreachable"));
    }
}
