//! Session title generation via LLM tool call.

use indexmap::IndexMap;

use crate::agent::config::{ModelEntry, find_model_by_id};
use crate::sampling::{
    Client as OaiCompatClient, ConversationItem, ConversationRequest, ConversationToolChoice,
    ToolSpec,
};
use crate::session::helpers::chat::floor_char_boundary;

/// Upper bound on the user text that feeds title generation; titles only need
/// the opening, and this keeps the request well under the model prompt limit.
const TITLE_SOURCE_MAX_BYTES: usize = 8_000;

#[derive(serde::Deserialize)]
struct SessionTitle {
    session_title: String,
}

/// Remove `<system-reminder>…</system-reminder>` blocks from `text` — they are
/// system-injected context (e.g. the `/goal` setup reminder), not the user's
/// words, so they must not drive the session title.
fn strip_system_reminder_blocks(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + OPEN.len()..];
        // An unterminated reminder drops the remainder — it is system text.
        let Some(end) = after_open.find(CLOSE) else {
            return out.trim().to_string();
        };
        rest = &after_open[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Text the session title is derived from: strip system reminders and skill XML
/// markup, then cap to the first few KB. Stripping runs before the cap so a
/// leading reminder larger than the cap is still removed.
fn title_source_text(user_message: &str) -> String {
    let without_reminders = strip_system_reminder_blocks(user_message);
    let base = if without_reminders.is_empty() {
        user_message
    } else {
        &without_reminders
    };
    let mut display =
        xai_grok_tools::implementations::skills::skill::extract_skill_display_text(base)
            .unwrap_or_else(|| base.to_string());
    display.truncate(floor_char_boundary(&display, TITLE_SOURCE_MAX_BYTES));
    display
}

/// Purpose: This function replaces a Fable title model with a configured Haiku
/// model. Fable models reject the forced `session_title` tool choice.
/// Input: The resolved title model id. The model catalog.
/// Output: The catalog key of a Haiku model, or `model_id` without change.
/// Method:
///     1. If `model_id` is not a Fable model, return `model_id`.
///     2. Find the visible Haiku models in the catalog.
///     3. Prefer a Haiku model with the same `base_url` and `api_backend`.
///     4. If not, use the first Haiku model.
///     5. If there is no Haiku model, return `model_id`.
/// Note: Step 5 keeps the old result: the request fails and the title falls
/// back to truncated user text.
pub(crate) fn resolve_title_model(model_id: &str, models: &IndexMap<String, ModelEntry>) -> String {
    let fable = find_model_by_id(models, model_id);
    let wire_model = fable.map_or(model_id, |entry| entry.info.model.as_str());
    if !_is_model_family(wire_model, "fable") {
        return model_id.to_owned();
    }
    let haikus: Vec<(&String, &ModelEntry)> = models
        .iter()
        .filter(|(_, entry)| !entry.info.hidden)
        .filter(|(_, entry)| _is_model_family(&entry.info.model, "haiku"))
        .collect();
    let same_provider = haikus.iter().find(|(_, entry)| {
        fable.is_some_and(|f| {
            f.info.base_url == entry.info.base_url && f.info.api_backend == entry.info.api_backend
        })
    });
    match same_provider.or(haikus.first()) {
        Some((key, _)) => (*key).clone(),
        None => model_id.to_owned(),
    }
}

pub(crate) fn title_fallback_from_user_text(user_message: &str) -> String {
    let text = title_source_text(user_message);
    let s = text
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ");
    if s.is_empty() {
        "New session".to_string()
    } else {
        s
    }
}

/// Generates a title for the session by looking at the first user message
/// We do not generate more of it on next user message unless its very important
///
/// Ideally we should be updating it as the session continues, but ... skipping that for now
pub async fn generate_session_summary(
    user_message: String,
    client: OaiCompatClient,
    model: &str,
) -> String {
    let clean_message = title_source_text(&user_message);
    let request = ConversationRequest::from_items(vec![
        ConversationItem::system(
            r#"You are tasked with generating the session title. The user is asking almost always software engineering related questions on their codebase.
We describe the session title below
# Session Title
A short and distinctive 5-10 word descriptive title for the session. Super info dense, no filler.

You will be given the user query below encapsulated in <user_query></user_query>.

Just generate the session_title and nothing else"#,
        ),
        ConversationItem::user(format!(
            r#"<user_query>
{}
</user_query>"#,
            clean_message
        )),
    ])
    .with_model(model)
    .with_tools(vec![ToolSpec {
        name: "session_title".to_owned(),
        description: Some("Generate the session_title which we use for the user_message".to_owned()),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["session_title"],
            "properties": {
                "session_title": {
                    "type": "string",
                    "description": "Final session title, just 5-10 word descriptive title for the session. Super info dense, no filler."
                }
            },
            "additionalProperties": false
        }),
    }])
    .with_max_output_tokens(100)
    .with_temperature(1.0)
    .with_tool_choice(ConversationToolChoice::Function("session_title".to_owned()));

    match client.conversation_collect(request).await {
        Ok(response) => {
            if let Some(a) = response.assistant()
                && let Some(tool_call) = a.tool_calls.first()
                && let Ok(result) = serde_json::from_str::<SessionTitle>(&tool_call.arguments)
            {
                return result.session_title;
            }
            tracing::debug!(
                model = %model,
                "session title generation: response did not contain a session_title tool call"
            );
        }
        Err(e) => {
            tracing::warn!(
                model = %model,
                error = %e,
                "session title generation failed, falling back to truncated user text"
            );
        }
    }
    title_fallback_from_user_text(&clean_message)
}

fn _is_model_family(wire_model: &str, family: &str) -> bool {
    wire_model.to_ascii_lowercase().contains(family)
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::{
        TITLE_SOURCE_MAX_BYTES, resolve_title_model, strip_system_reminder_blocks,
        title_fallback_from_user_text, title_source_text,
    };
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use crate::sampling::ApiBackend;

    const VERTEX_URL: &str = "https://aiplatform.googleapis.com";

    fn entry(model: &str, base_url: &str, api_backend: ApiBackend) -> ModelEntry {
        let mut entry = ModelEntry::fallback(model, &EndpointsConfig::default());
        entry.info.base_url = base_url.to_owned();
        entry.info.api_backend = api_backend;
        entry
    }

    fn catalog(entries: Vec<(&str, ModelEntry)>) -> IndexMap<String, ModelEntry> {
        entries
            .into_iter()
            .map(|(k, e)| (k.to_owned(), e))
            .collect()
    }

    #[test]
    fn fable_title_model_uses_haiku_on_same_provider() {
        let models = catalog(vec![
            (
                "other/claude-haiku-5-5",
                entry("claude-haiku-5-5", "https://x", ApiBackend::default()),
            ),
            (
                "vertex-claude/claude-fable-5-2",
                entry("claude-fable-5-2", VERTEX_URL, ApiBackend::VertexMessages),
            ),
            (
                "vertex-claude/claude-haiku-5-5",
                entry("claude-haiku-5-5", VERTEX_URL, ApiBackend::VertexMessages),
            ),
        ]);
        assert_eq!(
            resolve_title_model("vertex-claude/claude-fable-5-2", &models),
            "vertex-claude/claude-haiku-5-5"
        );
    }

    #[test]
    fn fable_title_model_uses_any_haiku_when_none_on_same_provider() {
        let models = catalog(vec![
            (
                "vertex-claude/claude-fable-5-2",
                entry("claude-fable-5-2", VERTEX_URL, ApiBackend::VertexMessages),
            ),
            (
                "other/claude-haiku-5-5",
                entry("claude-haiku-5-5", "https://x", ApiBackend::default()),
            ),
        ]);
        assert_eq!(
            resolve_title_model("vertex-claude/claude-fable-5-2", &models),
            "other/claude-haiku-5-5"
        );
    }

    #[test]
    fn fable_title_model_without_haiku_is_unchanged() {
        let mut hidden = entry("claude-haiku-5-5", VERTEX_URL, ApiBackend::VertexMessages);
        hidden.info.hidden = true;
        let models = catalog(vec![
            (
                "vertex-claude/claude-fable-5-2",
                entry("claude-fable-5-2", VERTEX_URL, ApiBackend::VertexMessages),
            ),
            (
                "vertex-claude/claude-sonnet-5-5",
                entry("claude-sonnet-5-5", VERTEX_URL, ApiBackend::VertexMessages),
            ),
            ("vertex-claude/hidden-haiku", hidden),
        ]);
        assert_eq!(
            resolve_title_model("vertex-claude/claude-fable-5-2", &models),
            "vertex-claude/claude-fable-5-2"
        );
        // A Fable slug absent from the catalog also passes through.
        assert_eq!(
            resolve_title_model("claude-fable-5-2", &IndexMap::new()),
            "claude-fable-5-2"
        );
    }

    #[test]
    fn non_fable_title_model_is_unchanged() {
        let models = catalog(vec![
            (
                "vertex-claude/claude-sonnet-5-5",
                entry("claude-sonnet-5-5", VERTEX_URL, ApiBackend::VertexMessages),
            ),
            (
                "vertex-claude/claude-haiku-5-5",
                entry("claude-haiku-5-5", VERTEX_URL, ApiBackend::VertexMessages),
            ),
        ]);
        for model_id in [
            "vertex-claude/claude-sonnet-5-5",
            "vertex-claude/claude-haiku-5-5",
            "grok-4",
        ] {
            assert_eq!(resolve_title_model(model_id, &models), model_id);
        }
    }

    #[test]
    fn title_source_text_caps_oversized_input() {
        let big = "word ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_cap_is_utf8_safe() {
        // 3-byte chars straddle the byte cap; must truncate on a boundary, not panic.
        let big = "あ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_strips_leading_reminder_larger_than_cap() {
        // A leading reminder bigger than the cap must still be stripped, so the
        // title derives from the objective rather than reminder text.
        let reminder = "x".repeat(TITLE_SOURCE_MAX_BYTES * 2);
        let input =
            format!("<system-reminder>\n{reminder}\n</system-reminder>\n\nbuild a mario game");
        let out = title_source_text(&input);
        assert_eq!(out, "build a mario game");
    }

    #[test]
    fn strip_removes_goal_setup_reminder_leaving_objective() {
        let input = "<system-reminder>\nA goal has been set: do stuff\nlots of rules\nStart \
                     now.\n</system-reminder>\n\nbuild a mario platformer game";
        assert_eq!(
            strip_system_reminder_blocks(input),
            "build a mario platformer game"
        );
    }

    #[test]
    fn strip_handles_unterminated_reminder() {
        assert_eq!(
            strip_system_reminder_blocks("<system-reminder>\nrules with no close tag"),
            ""
        );
    }

    #[test]
    fn strip_no_reminder_is_identity() {
        assert_eq!(
            strip_system_reminder_blocks("fix the auth bug"),
            "fix the auth bug"
        );
    }

    /// Regression: a `/goal <objective>` first turn must title off the
    /// objective, not the injected `<system-reminder>` setup block.
    #[test]
    fn fallback_titles_off_goal_objective_not_reminder() {
        let input = "<system-reminder>\nA goal has been set: do stuff\nStart \
                     now.\n</system-reminder>\n\nbuild a mario platformer game in html";
        assert_eq!(
            title_fallback_from_user_text(input),
            "build a mario platformer game in html"
        );
    }

    #[test]
    fn fallback_trims_to_words() {
        assert_eq!(
            title_fallback_from_user_text(
                "one two three four five six seven eight nine ten eleven"
            ),
            "one two three four five six seven eight nine ten"
        );
    }

    #[test]
    fn fallback_new_session_when_whitespace_only() {
        assert_eq!(title_fallback_from_user_text("   \n\t"), "New session");
    }

    #[test]
    fn fallback_strips_skill_xml_with_args() {
        let input = "<command-name>implement</command-name>\n\
                      <command-message>/implement</command-message>\n\
                      <command-args>fix the rendering bug</command-args>";
        assert_eq!(
            title_fallback_from_user_text(input),
            "/implement fix the rendering bug",
        );
    }

    #[test]
    fn fallback_strips_skill_xml_no_args() {
        let input = "<command-name>deploy</command-name>\n\
                      <command-message>/deploy</command-message>";
        assert_eq!(title_fallback_from_user_text(input), "/deploy");
    }

    #[test]
    fn fallback_plain_text_unaffected() {
        assert_eq!(
            title_fallback_from_user_text("fix the auth bug in login.rs"),
            "fix the auth bug in login.rs",
        );
    }
}
