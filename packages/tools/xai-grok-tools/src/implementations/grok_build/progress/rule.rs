//! Purpose: This module calculates the progress of a background command from
//! its output. A regex rule from the agent selects the output lines that carry
//! progress.
//!
//! The module is pure. It does not read files and it does not send
//! notifications.

use regex::Regex;

/// Maximum compiled size of a progress regex.
const REGEX_SIZE_LIMIT_BYTES: usize = 1 << 20;
/// Maximum length of a progress pattern from the model.
const MAX_PATTERN_LEN: usize = 512;
/// Maximum size of the partial line that the tracker keeps between chunks.
const MAX_PENDING_LINE_BYTES: usize = 8 * 1024;
/// The total of a rule that reports a percentage.
pub const PERCENT_TOTAL: u64 = 100;

/// One progress value: `completed` units of `total` units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressValue {
    pub completed: u64,
    pub total: u64,
}

impl ProgressValue {
    /// Purpose: Make a value that a progress bar can show.
    /// Input: The completed units and the total units.
    /// Output: `None` when the total is zero. If not, a value with
    /// `completed` not larger than `total`.
    pub fn new(completed: u64, total: u64) -> Option<Self> {
        if total == 0 {
            return None;
        }
        Some(Self {
            completed: completed.min(total),
            total,
        })
    }
}

/// How a matched line gives a progress value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleMode {
    /// Two capture groups: group 1 is the completed count, group 2 is the total.
    Fraction,
    /// One capture group and a fixed total: group 1 is the completed count.
    Value { total: u64 },
    /// One capture group and no total: group 1 is a percentage.
    Percent,
    /// No capture group and a fixed total: each matched line is one unit.
    Count { total: u64 },
}

/// A compiled progress rule.
#[derive(Debug, Clone)]
pub struct ProgressRule {
    regex: Regex,
    mode: RuleMode,
}

impl ProgressRule {
    /// Purpose: Compile a progress rule from the tool input.
    /// Input: The regex pattern and the optional total.
    /// Output: The rule, or a message for the model that tells what is wrong.
    /// Method:
    ///     1. Compile the pattern with a size limit.
    ///     2. Count the capture groups of the pattern.
    ///     3. Select the mode from the group count and the total.
    pub fn new(pattern: &str, total: Option<u64>) -> Result<Self, String> {
        if pattern.trim().is_empty() {
            return Err("the progress pattern is empty".to_string());
        }
        if pattern.len() > MAX_PATTERN_LEN {
            return Err(format!(
                "the progress pattern is longer than {MAX_PATTERN_LEN} bytes"
            ));
        }
        if total == Some(0) {
            return Err("the progress total must be larger than 0".to_string());
        }
        let regex = regex::RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE_LIMIT_BYTES)
            .build()
            .map_err(|e| format!("the progress pattern is not a valid regex: {e}"))?;
        let group_count = regex.captures_len() - 1;
        let mode = match (group_count, total) {
            (0, Some(total)) => RuleMode::Count { total },
            (0, None) => {
                return Err(
                    "a progress pattern with no capture group counts the matched lines, \
                     so it needs a total"
                        .to_string(),
                );
            }
            (1, Some(total)) => RuleMode::Value { total },
            (1, None) => RuleMode::Percent,
            (2, None) => RuleMode::Fraction,
            (2, Some(_)) => {
                return Err(
                    "a progress pattern with two capture groups reads the total from the \
                     output, so do not also set a total"
                        .to_string(),
                );
            }
            _ => {
                return Err(
                    "the progress pattern has more than two capture groups; use (?:...) for \
                     groups that do not carry a number"
                        .to_string(),
                );
            }
        };
        Ok(Self { regex, mode })
    }

    /// The value to show before the first match, when the total is known.
    pub fn initial_value(&self) -> Option<ProgressValue> {
        match self.mode {
            RuleMode::Fraction => None,
            RuleMode::Value { total } | RuleMode::Count { total } => ProgressValue::new(0, total),
            RuleMode::Percent => ProgressValue::new(0, PERCENT_TOTAL),
        }
    }
}

/// Purpose: This struct follows the output of one command and keeps the
/// latest progress value.
#[derive(Debug)]
pub struct ProgressTracker {
    rule: ProgressRule,
    /// The end of the output that has no line terminator yet.
    pending_line: String,
    /// The number of matched lines (used in count mode).
    matched_line_count: u64,
    latest: Option<ProgressValue>,
}

impl ProgressTracker {
    pub fn new(rule: ProgressRule) -> Self {
        let latest = rule.initial_value();
        Self {
            rule,
            pending_line: String::new(),
            matched_line_count: 0,
            latest,
        }
    }

    /// The latest progress value, if there is one.
    pub fn latest(&self) -> Option<ProgressValue> {
        self.latest
    }

    /// Purpose: Read new output and update the progress value.
    /// Input: The new output text. The text can end in the middle of a line.
    /// Output: The new value when the value changed. If not, `None`.
    /// Method:
    ///     1. Add the text to the partial line from the last call.
    ///     2. Split at `\n` and `\r`. Progress bars often redraw with `\r`.
    ///     3. Apply the rule to each complete line.
    ///     4. Keep the last partial line for the next call.
    pub fn push(&mut self, chunk: &str) -> Option<ProgressValue> {
        let before = self.latest;
        self.pending_line.push_str(chunk);
        let text = std::mem::take(&mut self.pending_line);
        let mut rest = text.as_str();
        while let Some(end) = rest.find(['\n', '\r']) {
            self.apply_line(&rest[..end]);
            rest = &rest[end + 1..];
        }
        self.pending_line = tail_within(rest, MAX_PENDING_LINE_BYTES).to_string();
        (self.latest != before).then_some(self.latest).flatten()
    }

    /// Purpose: Apply the rule to the partial last line when the command ends.
    /// Output: The new value when the value changed. If not, `None`.
    pub fn finish(&mut self) -> Option<ProgressValue> {
        let before = self.latest;
        let line = std::mem::take(&mut self.pending_line);
        self.apply_line(&line);
        (self.latest != before).then_some(self.latest).flatten()
    }

    fn apply_line(&mut self, line: &str) {
        let Some(captures) = self.rule.regex.captures(line) else {
            return;
        };
        let group = |index: usize| captures.get(index).and_then(|m| parse_number(m.as_str()));
        let value = match self.rule.mode {
            RuleMode::Fraction => match (group(1), group(2)) {
                (Some(completed), Some(total)) => ProgressValue::new(completed, total),
                _ => None,
            },
            RuleMode::Value { total } => group(1).and_then(|c| ProgressValue::new(c, total)),
            RuleMode::Percent => group(1).and_then(|c| ProgressValue::new(c, PERCENT_TOTAL)),
            RuleMode::Count { total } => {
                self.matched_line_count += 1;
                ProgressValue::new(self.matched_line_count, total)
            }
        };
        if value.is_some() {
            self.latest = value;
        }
    }
}

/// Purpose: Parse a number from a capture group.
/// Input: Text such as `12`, `1,024` or `45.6`.
/// Output: The number, rounded to an integer. `None` when the text is not a
/// number that is zero or larger.
fn parse_number(text: &str) -> Option<u64> {
    let cleaned: String = text.trim().chars().filter(|c| *c != ',').collect();
    let number: f64 = cleaned.parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    Some(number.round() as u64)
}

/// The last part of `text` that is not longer than `max_bytes`.
fn tail_within(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(pattern: &str, total: Option<u64>) -> ProgressTracker {
        ProgressTracker::new(ProgressRule::new(pattern, total).expect("valid rule"))
    }

    fn value(completed: u64, total: u64) -> Option<ProgressValue> {
        Some(ProgressValue { completed, total })
    }

    #[test]
    fn fraction_rule_reads_completed_and_total_from_the_line() {
        let mut t = tracker(r"(\d+)/(\d+) tests", None);
        assert_eq!(t.latest(), None);
        assert_eq!(t.push("ran 3/40 tests\n"), value(3, 40));
        assert_eq!(t.push("noise\n"), None);
        assert_eq!(t.push("ran 40/40 tests\n"), value(40, 40));
    }

    #[test]
    fn percent_rule_starts_at_zero_and_rounds() {
        let mut t = tracker(r"\[\s*(\d+(?:\.\d+)?)%\]", None);
        assert_eq!(t.latest(), value(0, 100));
        assert_eq!(t.push("tests/a.py ... [ 45.6%]\n"), value(46, 100));
    }

    #[test]
    fn value_rule_uses_the_fixed_total() {
        let mut t = tracker(r"^Epoch (\d+)", Some(10));
        assert_eq!(t.latest(), value(0, 10));
        assert_eq!(t.push("Epoch 4 loss=0.2\n"), value(4, 10));
        // A value above the total does not overflow the bar.
        assert_eq!(t.push("Epoch 12 loss=0.1\n"), value(10, 10));
    }

    #[test]
    fn count_rule_counts_each_matched_line_once() {
        let mut t = tracker(r"^Compiling ", Some(4));
        assert_eq!(t.push("Compiling a\nCompiling b\nwarning: x\n"), value(2, 4));
        assert_eq!(t.push("Compil"), None);
        assert_eq!(t.push("ing c\n"), value(3, 4));
    }

    #[test]
    fn carriage_return_redraws_are_separate_lines() {
        let mut t = tracker(r"(\d+)%", None);
        assert_eq!(t.push("10%\r20%\r30%"), value(20, 100));
        assert_eq!(t.finish(), value(30, 100));
    }

    #[test]
    fn unchanged_value_returns_none() {
        let mut t = tracker(r"(\d+)%", None);
        assert_eq!(t.push("50%\n"), value(50, 100));
        assert_eq!(t.push("50%\n"), None);
    }

    #[test]
    fn numbers_with_thousands_separators_parse() {
        let mut t = tracker(r"([\d,]+) of ([\d,]+)", None);
        assert_eq!(t.push("1,024 of 4,096\n"), value(1024, 4096));
    }

    #[test]
    fn zero_total_in_the_output_is_ignored() {
        let mut t = tracker(r"(\d+)/(\d+)", None);
        assert_eq!(t.push("0/0\n"), None);
    }

    #[test]
    fn invalid_rules_are_rejected_with_a_reason() {
        assert!(ProgressRule::new("(", None).unwrap_err().contains("regex"));
        assert!(ProgressRule::new("done", None).unwrap_err().contains("total"));
        assert!(ProgressRule::new(r"(\d+)/(\d+)", Some(5)).is_err());
        assert!(ProgressRule::new(r"(\d+)(\d+)(\d+)", None).is_err());
        assert!(ProgressRule::new(r"(\d+)", Some(0)).is_err());
        assert!(ProgressRule::new("  ", Some(3)).is_err());
    }

    #[test]
    fn long_partial_line_is_bounded() {
        let mut t = tracker(r"(\d+)%", None);
        t.push(&"x".repeat(MAX_PENDING_LINE_BYTES * 2));
        assert!(t.pending_line.len() <= MAX_PENDING_LINE_BYTES);
    }
}
