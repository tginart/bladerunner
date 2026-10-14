//! Advisory, task-specific deadlines for top-level user turns.
//!
//! A deadline is stamped once from the top-level prompt ID, written to the
//! workspace, appended to the model-facing prompt, and watched by one
//! cancellable timer. Tool calls, continuation steps, and child agents reuse
//! the parent's instruction and never create their own deadline.

use super::*;

const EXPIRY_TEXT_PREFIX: &str = "The deadline for task";

#[derive(Debug, Default)]
pub(crate) struct SessionDeadline {
    pub config: crate::agent::config::DeadlineConfig,
    active: std::cell::RefCell<Option<ActiveDeadline>>,
}

#[derive(Debug)]
struct ActiveDeadline {
    prompt_id: String,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedDeadline {
    pub task_id: String,
    pub path: std::path::PathBuf,
    pub duration_secs: u64,
    pub deadline_unix: u64,
    pub reasoning_effort: Option<String>,
}

pub(crate) fn parse_duration(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.len() < 2 {
        return Err("duration must be a positive integer followed by s, m, or h".into());
    }
    let (number, unit) = value.split_at(value.len() - 1);
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err("duration must be a positive integer followed by s, m, or h".into());
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| "duration is too large".to_string())?;
    if amount == 0 {
        return Err("duration must be greater than zero".into());
    }
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        _ => return Err("duration unit must be s, m, or h".into()),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_string())
}

fn safe_task_id(prompt_id: &str) -> String {
    let sanitized: String = prompt_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "task".to_string()
    } else {
        sanitized
    }
}

impl SessionDeadline {
    pub(crate) fn new(config: crate::agent::config::DeadlineConfig) -> Self {
        Self {
            config,
            active: std::cell::RefCell::new(None),
        }
    }

    fn configured_duration(
        &self,
        effort: Option<xai_grok_sampling_types::ReasoningEffort>,
    ) -> &str {
        effort
            .and_then(|value| self.config.by_effort.get(&value.to_string()))
            .map(String::as_str)
            .unwrap_or(self.config.default.as_str())
    }

    pub(crate) fn prepare(
        &self,
        cwd: &std::path::Path,
        prompt_id: &str,
        effort: Option<xai_grok_sampling_types::ReasoningEffort>,
        manual_duration: Option<&str>,
    ) -> Result<Option<PreparedDeadline>, String> {
        if manual_duration.is_none() && !self.config.enabled {
            return Ok(None);
        }
        let duration_secs =
            parse_duration(manual_duration.unwrap_or_else(|| self.configured_duration(effort)))?;
        let deadline_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before the Unix epoch".to_string())?
            .as_secs()
            .checked_add(duration_secs)
            .ok_or_else(|| "deadline timestamp overflow".to_string())?;
        let task_id = prompt_id.to_string();
        let path = cwd.join(format!("DEADLINE.{}.txt", safe_task_id(prompt_id)));
        Ok(Some(PreparedDeadline {
            task_id,
            path,
            duration_secs,
            deadline_unix,
            reasoning_effort: effort.map(|v| v.to_string()),
        }))
    }

    pub(crate) async fn write_and_append(
        &self,
        prepared: &PreparedDeadline,
        prompt_blocks: &mut Vec<acp::ContentBlock>,
    ) -> Result<(), String> {
        let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(prepared.deadline_unix as i64, 0)
            .ok_or_else(|| "deadline timestamp is outside the supported range".to_string())?
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut file = format!(
            "task_id {}\ndeadline_unix {}\ndeadline_utc {}\nduration_seconds {}\n",
            prepared.task_id, prepared.deadline_unix, utc, prepared.duration_secs
        );
        file.push_str(&format!(
            "reasoning_effort {}\n",
            prepared.reasoning_effort.as_deref().unwrap_or("inherit")
        ));
        file.push_str("remaining_seconds_formula deadline_unix - date +%s\n");
        tokio::fs::write(&prepared.path, file)
            .await
            .map_err(|e| format!("failed to write {}: {e}", prepared.path.display()))?;

        let instruction = format!(
            "Complete task `{}` before its deadline. The deadline for this task is stored at `{}`. Use only this deadline file for task `{}`. `deadline_unix` is the absolute deadline; remaining seconds are `deadline_unix - date +%s`.",
            prepared.task_id,
            prepared.path.display(),
            prepared.task_id,
        );
        prompt_blocks.push(acp::ContentBlock::Text(acp::TextContent::new(instruction)));
        Ok(())
    }

    pub(crate) fn cancel_for(&self, prompt_id: &str) {
        let mut active = self.active.borrow_mut();
        if active
            .as_ref()
            .is_some_and(|item| item.prompt_id == prompt_id)
        {
            if let Some(item) = active.take() {
                item.cancel.cancel();
            }
        }
    }
}

impl SessionActor {
    pub(crate) fn arm_deadline(self: &std::sync::Arc<Self>, prepared: &PreparedDeadline) {
        if let Some(previous) = self.deadline.active.borrow_mut().take() {
            previous.cancel.cancel();
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        self.deadline.active.borrow_mut().replace(ActiveDeadline {
            prompt_id: prepared.task_id.clone(),
            cancel: cancel.clone(),
        });
        let actor = std::sync::Arc::downgrade(self);
        let task_id = prepared.task_id.clone();
        let wait = std::time::Duration::from_secs(prepared.duration_secs);
        tokio::task::spawn_local(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep(wait) => {
                    let Some(actor) = actor.upgrade() else { return; };
                    let is_current = actor.current_prompt_id.lock().ok()
                        .and_then(|guard| guard.clone())
                        .as_deref() == Some(task_id.as_str());
                    if !is_current { return; }
                    let text = format!(
                        "{EXPIRY_TEXT_PREFIX} `{task_id}` has arrived. Prepare your final response now. If the task is incomplete, report completed work, partial results, and remaining work."
                    );
                    actor.broadcast_interjection(&text, None);
                    actor.pending_interjections.push(PendingInterjection {
                        text,
                        attachments: vec![],
                    });
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_positive_durations() {
        assert_eq!(parse_duration("30s"), Ok(30));
        assert_eq!(parse_duration("20m"), Ok(1200));
        assert_eq!(parse_duration("2h"), Ok(7200));
    }

    #[test]
    fn rejects_invalid_durations() {
        for value in ["", "0s", "20", "-1m", "1h30m", "five minutes", "1d"] {
            assert!(parse_duration(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn sanitizes_only_the_filename_component() {
        assert_eq!(safe_task_id("prompt/one:two"), "prompt_one_two");
    }

    #[tokio::test]
    async fn writes_task_file_and_appends_matching_instruction() {
        let temp = tempfile::tempdir().unwrap();
        let deadline = SessionDeadline::new(crate::agent::config::DeadlineConfig::default());
        let prepared = deadline
            .prepare(temp.path(), "prompt/42", None, Some("30s"))
            .unwrap()
            .unwrap();
        let mut blocks = vec![];
        deadline
            .write_and_append(&prepared, &mut blocks)
            .await
            .unwrap();
        assert_eq!(prepared.path, temp.path().join("DEADLINE.prompt_42.txt"));
        let file = tokio::fs::read_to_string(&prepared.path).await.unwrap();
        assert!(file.contains("task_id prompt/42"));
        assert!(file.contains("duration_seconds 30"));
        let acp::ContentBlock::Text(text) = &blocks[0] else {
            panic!("expected text block")
        };
        assert!(text.text.contains("task `prompt/42`"));
        assert!(text.text.contains(prepared.path.to_str().unwrap()));
    }

    #[test]
    fn effort_override_beats_general_default() {
        let mut config = crate::agent::config::DeadlineConfig {
            enabled: true,
            default: "20m".into(),
            ..Default::default()
        };
        config.by_effort.insert("high".into(), "60m".into());
        let deadline = SessionDeadline::new(config);
        let prepared = deadline
            .prepare(
                std::path::Path::new("/tmp"),
                "p1",
                Some(xai_grok_sampling_types::ReasoningEffort::High),
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(prepared.duration_secs, 3600);
    }
}
