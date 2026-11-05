//! JSON and Server-Sent Events endpoints.

use std::convert::Infallible;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::{HOST, ORIGIN};
use axum::response::Redirect;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use futures_util::Stream;
use serde::Deserialize;

use crate::store::{
    ChatMessage, DashboardStore, LogEntry, ResourceMetrics, ServerMetrics, SessionCharts,
    SessionDetail, SessionPage, SessionQuery, TimelineEvent,
};

const MAX_LIVE_BATCH_BYTES: u64 = 1024 * 1024;

pub type StoreState = Arc<DashboardStore>;
type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

struct LiveStreamState {
    path: std::path::PathBuf,
    offset: u64,
    pending: std::collections::VecDeque<String>,
    interval: tokio::time::Interval,
}

#[derive(Debug)]
pub struct ApiError(anyhow::Error);

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::warn!(error = %self.0, "dashboard API request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LimitQuery {
    pub limit: usize,
}

impl Default for LimitQuery {
    fn default() -> Self {
        Self { limit: 500 }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LogsQuery {
    pub limit: Option<usize>,
    pub level: Option<String>,
    pub session_id: Option<String>,
}

pub async fn list_sessions(
    State(store): State<StoreState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<SessionPage>, ApiError> {
    Ok(Json(store.query_sessions(query).await?))
}

pub async fn get_session(
    State(store): State<StoreState>,
    Path(id): Path<String>,
) -> Result<Json<Option<SessionDetail>>, ApiError> {
    Ok(Json(store.get_session(&id).await?))
}

pub async fn get_timeline(
    State(store): State<StoreState>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<Vec<TimelineEvent>>, ApiError> {
    Ok(Json(store.get_timeline(&id, query.limit).await?))
}

pub async fn get_chat_history(
    State(store): State<StoreState>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<Vec<ChatMessage>>, ApiError> {
    Ok(Json(store.get_chat_history(&id, query.limit).await?))
}

pub async fn get_session_charts(
    State(store): State<StoreState>,
    Path(id): Path<String>,
) -> Result<Json<Option<SessionCharts>>, ApiError> {
    Ok(Json(store.session_charts(&id).await?))
}

pub async fn get_server_metrics(
    State(store): State<StoreState>,
) -> Result<Json<ServerMetrics>, ApiError> {
    Ok(Json(store.server_metrics().await?))
}

pub async fn get_resource_metrics(
    State(store): State<StoreState>,
) -> Result<Json<ResourceMetrics>, ApiError> {
    Ok(Json(store.resource_metrics().await?))
}

pub async fn get_logs(
    State(store): State<StoreState>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<Vec<LogEntry>>, ApiError> {
    Ok(Json(
        store
            .get_logs(
                query.limit.unwrap_or(500),
                query.level.as_deref(),
                query.session_id.as_deref(),
            )
            .await?,
    ))
}

/// Stream newly appended lines from one session's events.jsonl. The stream
/// starts at EOF so opening the page never replays an unbounded historical log.
pub async fn live_events(
    State(store): State<StoreState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let path = store
        .session_file(&id, "events.jsonl")
        .await?
        .ok_or_else(|| anyhow::anyhow!("session events not found"))?;
    let initial_offset = std::fs::metadata(&path)?.len();

    let mut interval = tokio::time::interval(Duration::from_millis(750));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let state = LiveStreamState {
        path,
        offset: initial_offset,
        pending: std::collections::VecDeque::new(),
        interval,
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(line) = state.pending.pop_front() {
                let event_name = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "event".to_owned());
                return Some((Ok(Event::default().event(event_name).data(line)), state));
            }

            state.interval.tick().await;
            let path = state.path.clone();
            let offset = state.offset;
            let result =
                tokio::task::spawn_blocking(move || read_appended_lines(&path, offset)).await;
            let Ok(Ok((next_offset, lines))) = result else {
                continue;
            };
            state.offset = next_offset;
            state.pending.extend(lines);
        }
    });

    Ok(Sse::new(Box::pin(stream) as EventStream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn read_appended_lines(
    path: &std::path::Path,
    requested_offset: u64,
) -> std::io::Result<(u64, Vec<String>)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let requested_offset = if len < requested_offset {
        0
    } else {
        requested_offset
    };
    let backlog = len.saturating_sub(requested_offset);
    let dropped_backlog = backlog > MAX_LIVE_BATCH_BYTES;
    let read_start = if dropped_backlog {
        len - MAX_LIVE_BATCH_BYTES
    } else {
        requested_offset
    };

    file.seek(SeekFrom::Start(read_start))?;
    let mut bytes = Vec::with_capacity(MAX_LIVE_BATCH_BYTES.min(len - read_start) as usize);
    file.take(MAX_LIVE_BATCH_BYTES).read_to_end(&mut bytes)?;

    // If producers outran the client, retain only the bounded tail and discard
    // the first partial JSONL record. Live observation favors recent events
    // over replaying an arbitrarily large backlog into memory.
    let skipped_prefix = if dropped_backlog {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok((read_start + bytes.len() as u64, Vec::new()));
        };
        first_newline + 1
    } else {
        0
    };
    let bytes = &bytes[skipped_prefix..];
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok((read_start + skipped_prefix as u64, Vec::new()));
    };
    let complete = &bytes[..=last_newline];
    let next_offset = read_start + skipped_prefix as u64 + complete.len() as u64;
    let lines = String::from_utf8_lossy(complete)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    Ok((next_offset, lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn appended_reader_keeps_partial_line_for_next_read() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"one\ntwo").unwrap();
        let (offset, lines) = read_appended_lines(temp.path(), 0).unwrap();
        assert_eq!(offset, 4);
        assert_eq!(lines, ["one"]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(temp.path())
            .unwrap();
        writeln!(file, "-done").unwrap();
        let (offset, lines) = read_appended_lines(temp.path(), offset).unwrap();
        assert_eq!(offset, std::fs::metadata(temp.path()).unwrap().len());
        assert_eq!(lines, ["two-done"]);
    }

    #[test]
    fn appended_reader_keeps_only_a_bounded_backlog_tail() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let event = r#"{"type":"turn_started"}"#;
        let mut bytes = vec![b'x'; MAX_LIVE_BATCH_BYTES as usize + 128];
        bytes.push(b'\n');
        bytes.extend_from_slice(event.as_bytes());
        bytes.push(b'\n');
        std::fs::write(temp.path(), bytes).unwrap();

        let (offset, lines) = read_appended_lines(temp.path(), 0).unwrap();
        assert_eq!(offset, std::fs::metadata(temp.path()).unwrap().len());
        assert_eq!(lines, [event]);
    }
}

/// `GET /api/hubs`: visible agent hubs.
pub async fn list_hubs(
    State(store): State<StoreState>,
) -> Result<Json<Vec<crate::hubs::HubSummary>>, ApiError> {
    let home = store.grok_home().to_path_buf();
    let hubs = tokio::task::spawn_blocking(move || crate::hubs::list(&home)).await??;
    Ok(Json(hubs))
}

#[derive(Debug, Deserialize)]
pub struct HubPostForm {
    pub to: String,
    pub text: String,
}

/// Purpose: Post to a hub as the person, from the hub page form.
/// Input: The hub name, the request headers and the form fields.
/// Output: A redirect back to the hub page, or an error status.
/// Note: A browser post from another origin is refused. Without this check,
///     any web page could plant messages that agents then read.
pub async fn post_hub_message(
    State(store): State<StoreState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HubPostForm>,
) -> Response {
    if !is_same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin post refused").into_response();
    }
    let home = store.grok_home().to_path_buf();
    let hub = name.clone();
    let result =
        tokio::task::spawn_blocking(move || crate::hubs::post(&home, &hub, &form.to, &form.text))
            .await;
    match result {
        Ok(Ok(_)) => Redirect::to(&format!("/hubs/{name}")).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// A request with no `Origin` header (curl, a local script) is allowed; a
/// browser always sends one with a form post.
fn is_same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    !host.is_empty() && origin.to_str().ok() == Some(format!("http://{host}").as_str())
}

#[cfg(test)]
mod hub_post_tests {
    use super::*;

    #[test]
    fn same_origin_check() {
        let mut headers = HeaderMap::new();
        assert!(is_same_origin(&headers));
        headers.insert(HOST, "127.0.0.1:9090".parse().unwrap());
        headers.insert(ORIGIN, "http://127.0.0.1:9090".parse().unwrap());
        assert!(is_same_origin(&headers));
        headers.insert(ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!is_same_origin(&headers));
        headers.insert(ORIGIN, "null".parse().unwrap());
        assert!(!is_same_origin(&headers));
    }
}
