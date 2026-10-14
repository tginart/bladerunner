//! Shell-backed [`HyperHost`] for Hypercore integration.
//!
//! Holds a live [`SamplerConfig`] (credentials + model route from the session)
//! and persists core snapshots under `{grok_home}/hypercore/`.
//!
//! Session turns use the legacy loop by default. Hypercore is experimental and
//! requires an explicit `HYPERCORE_TURN=1` opt-in.
//! Tool execution for opted-in agent turns goes through
//! [`HyperCore::submit_turn_with_tools`] + shell `execute_tool_calls` (P3),
//! not [`HyperHost::invoke_tool`] on this host (still unsupported for native
//! host-only demos).
//!
//! See `docs/design-hypercore.md` § existing-shell integration.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use xai_grok_sampler::SamplerConfig;
use xai_grok_sampling_types::{
    AssistantItem, ConversationItem, RequestEvidence, RequestEvidenceSink,
};
use xai_hyper_core::disk_store::HypercoreSessionStore;
use xai_hyper_core::native::open_model_stream_with_recording;
use xai_hyper_host::{
    HostError, HostToolCall, HostToolResult, HyperHost, ModelStream, ModelStreamRequest,
    TerminalTurnRecord,
};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TerminalProvenance {
    assistant: AssistantItem,
    evidence: RequestEvidence,
}

#[derive(Debug)]
struct CapturingEvidenceSink {
    captured: Arc<Mutex<Option<RequestEvidence>>>,
    downstream: Option<Arc<dyn RequestEvidenceSink>>,
}
impl RequestEvidenceSink for CapturingEvidenceSink {
    fn record(&self, evidence: &RequestEvidence) {
        *self.captured.lock().expect("evidence lock") = Some(evidence.clone());
        if let Some(sink) = &self.downstream {
            sink.record(evidence);
        }
    }
}

/// Shell implementation of [`HyperHost`].
///
/// Construct with a full session [`SamplerConfig`] (e.g. from
/// `reconstruct_full_config`). Call [`Self::replace_sampling_config`] when the
/// session model / credentials change.
#[derive(Clone)]
pub struct ShellHyperHost {
    sampling: Arc<RwLock<SamplerConfig>>,
    store: HypercoreSessionStore,
    stream_opens: Arc<AtomicU64>,
    evidence_sink: Option<Arc<dyn RequestEvidenceSink>>,
    accepted_assistant: Arc<Mutex<Option<AssistantItem>>>,
    request_evidence: Arc<Mutex<Option<RequestEvidence>>>,
}

impl std::fmt::Debug for ShellHyperHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellHyperHost")
            .field("storage_root", &self.store.root())
            .field("stream_opens", &self.stream_opens.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ShellHyperHost {
    /// Create a host with an initial sampling config and storage root.
    ///
    /// `storage_root` is typically `{grok_home}/hypercore`.
    pub fn new(sampling: SamplerConfig, storage_root: impl Into<PathBuf>) -> Self {
        Self {
            sampling: Arc::new(RwLock::new(sampling)),
            store: HypercoreSessionStore::new(storage_root.into()),
            stream_opens: Arc::new(AtomicU64::new(0)),
            evidence_sink: None,
            accepted_assistant: Arc::new(Mutex::new(None)),
            request_evidence: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach the session's durable evidence writer to every model step.
    pub(crate) fn with_evidence_sink(mut self, sink: Arc<dyn RequestEvidenceSink>) -> Self {
        self.evidence_sink = Some(sink);
        self
    }

    /// Transfer provenance from the accepted stream into its shell assistant row.
    pub(crate) fn stamp_assistant(&self, item: &mut ConversationItem) {
        if let ConversationItem::Assistant(assistant) = item
            && let Some(recorded) = self
                .accepted_assistant
                .lock()
                .expect("response lock")
                .as_ref()
        {
            assistant.record_request(
                recorded.tools_sha256.as_deref(),
                recorded.system_sha256.as_deref(),
                recorded.reasoning_effort,
            );
            assistant.hosted_tools.clone_from(&recorded.hosted_tools);
            assistant
                .reasoning_model_identity
                .clone_from(&recorded.reasoning_model_identity);
            assistant.model_id.clone_from(&recorded.model_id);
            assistant
                .model_fingerprint
                .clone_from(&recorded.model_fingerprint);
        }
    }

    /// Create under `{grok_home}/hypercore`.
    pub fn under_grok_home(sampling: SamplerConfig, grok_home: impl AsRef<Path>) -> Self {
        Self::new(sampling, grok_home.as_ref().join("hypercore"))
    }

    /// Replace the sampling config (model switch, re-auth, etc.).
    pub fn replace_sampling_config(&self, sampling: SamplerConfig) {
        *self.sampling.write().expect("sampling lock") = sampling;
    }

    /// Snapshot of the current sampling config (includes secrets — host only).
    pub fn sampling_config(&self) -> SamplerConfig {
        self.sampling.read().expect("sampling lock").clone()
    }

    /// Storage root (`…/hypercore`).
    pub fn storage_root(&self) -> &Path {
        self.store.root()
    }

    /// Number of times a model stream was opened (not counting idempotent replays).
    pub fn model_stream_opens(&self) -> u64 {
        self.stream_opens.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HyperHost for ShellHyperHost {
    async fn open_model_stream(
        &self,
        req: ModelStreamRequest,
    ) -> Result<Box<dyn ModelStream>, HostError> {
        let cfg = self.sampling_config();
        if cfg.api_key.is_none()
            && cfg.bearer_resolver.is_none()
            && !cfg
                .extra_headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            return Err(HostError::Message(
                "ShellHyperHost: SamplerConfig has no api_key, bearer_resolver, or Authorization header"
                    .into(),
            ));
        }
        self.stream_opens.fetch_add(1, Ordering::SeqCst);
        *self.accepted_assistant.lock().expect("response lock") = None;
        *self.request_evidence.lock().expect("evidence lock") = None;
        let accepted = self.accepted_assistant.clone();
        open_model_stream_with_recording(
            cfg,
            req,
            Some(Arc::new(CapturingEvidenceSink {
                captured: self.request_evidence.clone(),
                downstream: self.evidence_sink.clone(),
            })),
            Some(Arc::new(move |response| {
                *accepted.lock().expect("response lock") =
                    response.items.iter().rev().find_map(|item| {
                        if let ConversationItem::Assistant(assistant) = item {
                            Some(assistant.clone())
                        } else {
                            None
                        }
                    });
            })),
        )
    }

    async fn commit_snapshot(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError> {
        let mut terminal = terminal.cloned();
        if let Some(terminal) = &mut terminal {
            let assistant = self
                .accepted_assistant
                .lock()
                .expect("response lock")
                .clone();
            let evidence = self.request_evidence.lock().expect("evidence lock").clone();
            if let (Some(assistant), Some(evidence)) = (assistant, evidence) {
                terminal.provenance = Some(
                    serde_json::to_value(TerminalProvenance {
                        assistant,
                        evidence,
                    })
                    .map_err(|e| HostError::Message(e.to_string()))?,
                );
            }
        }
        self.store
            .commit_snapshot(session_id, snapshot, terminal.as_ref())
            .await
    }

    async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError> {
        self.store.load_snapshot(session_id).await
    }

    async fn load_terminal_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError> {
        let terminal = self.store.load_terminal_turn(session_id, turn_id).await?;
        *self.accepted_assistant.lock().expect("response lock") = None;
        *self.request_evidence.lock().expect("evidence lock") = None;
        if let Some(value) = terminal
            .as_ref()
            .and_then(|record| record.provenance.clone())
        {
            let recorded: TerminalProvenance = serde_json::from_value(value)
                .map_err(|e| HostError::Message(format!("invalid terminal provenance: {e}")))?;
            if let Some(sink) = &self.evidence_sink {
                sink.record(&recorded.evidence);
            }
            *self.request_evidence.lock().expect("evidence lock") = Some(recorded.evidence);
            *self.accepted_assistant.lock().expect("response lock") = Some(recorded.assistant);
        }
        Ok(terminal)
    }

    async fn invoke_tool(&self, _call: HostToolCall) -> Result<HostToolResult, HostError> {
        // Phase 1.5: tools stay on the legacy session path until wired.
        Err(HostError::Unsupported("shell tools via HyperHost"))
    }

    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_hyper_core::{CoreConfig, HyperCore, TurnRequest};
    use xai_hyper_host::HyperHost;

    fn test_sampler_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key-not-used-on-replay".into()),
            base_url: "http://127.0.0.1:9".into(),
            model: "test-model".into(),
            context_window: 128_000,
            ..Default::default()
        }
    }

    #[test]
    fn stream_provenance_reaches_plain_and_tool_assistants() {
        let dir = tempfile::tempdir().unwrap();
        let host = ShellHyperHost::new(test_sampler_config(), dir.path());
        let client = xai_grok_sampler::SamplingClient::new(test_sampler_config()).unwrap();
        let mut request = xai_grok_sampling_types::ConversationRequest::from_items(vec![
            ConversationItem::system("effective"),
        ]);
        let metadata = client.prepare_request_recording(&mut request).unwrap();
        *host.accepted_assistant.lock().unwrap() = Some(metadata.clone());
        for mut item in [
            ConversationItem::assistant("final text"),
            ConversationItem::assistant_tool_calls(vec![xai_grok_sampling_types::ToolCall {
                id: "call-1".into(),
                name: "tool".into(),
                arguments: "{}".into(),
            }]),
        ] {
            let before = serde_json::to_value(&item).unwrap();
            host.stamp_assistant(&mut item);
            let ConversationItem::Assistant(assistant) = &item else {
                panic!()
            };
            assert_eq!(assistant.tools_sha256, metadata.tools_sha256);
            assert_eq!(assistant.system_sha256, metadata.system_sha256);
            assert_eq!(assistant.hosted_tools, Some(vec![]));
            let after = serde_json::to_value(&item).unwrap();
            assert_eq!(before["content"], after["content"]);
            assert_eq!(before["tool_calls"], after["tool_calls"]);
        }
    }

    #[tokio::test]
    async fn shell_host_disk_restore_and_idempotent_turn() {
        let dir = tempfile::tempdir().unwrap();
        let host = ShellHyperHost::new(test_sampler_config(), dir.path().join("hypercore"));

        let evidence = RequestEvidence::new(&[], Some("original instructions".into()));
        let mut assistant = match ConversationItem::assistant("hello from shell host") {
            ConversationItem::Assistant(item) => item,
            _ => unreachable!(),
        };
        assistant.record_request(
            Some(&evidence.tools.0),
            evidence.system.as_ref().map(|s| s.0.as_str()),
            Some(xai_grok_sampling_types::ReasoningEffort::High),
        );
        assistant.hosted_tools = Some(vec![serde_json::json!({"type":"web_search"})]);
        assistant.model_id = Some("original-model".into());
        *host.accepted_assistant.lock().unwrap() = Some(assistant.clone());
        *host.request_evidence.lock().unwrap() = Some(evidence.clone());
        let session = "shell-sess-1";
        let snap = br#"{"schema_version":1,"session_id":"shell-sess-1","items":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello from shell host"}],"completed_turns":1,"model":"test-model","extensions":{}}"#;
        let term = TerminalTurnRecord {
            turn_id: "t1".into(),
            request_text: "hi".into(),
            assistant_text: "hello from shell host".into(),
            stop_reason: Some("end_turn".into()),
            provenance: None,
        };
        host.commit_snapshot(session, snap, Some(&term))
            .await
            .unwrap();

        drop(host);
        #[derive(Debug, Default)]
        struct Sink(Mutex<Vec<RequestEvidence>>);
        impl RequestEvidenceSink for Sink {
            fn record(&self, evidence: &RequestEvidence) {
                self.0.lock().unwrap().push(evidence.clone());
            }
        }
        let sink = Arc::new(Sink::default());
        let mut changed = test_sampler_config();
        changed.model = "changed-model".into();
        let host = ShellHyperHost::new(changed, dir.path().join("hypercore"))
            .with_evidence_sink(sink.clone());
        let mut core = HyperCore::restore_or_new(
            host.clone(),
            session,
            CoreConfig {
                model: "test-model".into(),
                max_messages: 64,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();
        assert_eq!(core.completed_turns(), 1);

        let opens_before = host.model_stream_opens();
        let out = core
            .submit_turn(TurnRequest {
                turn_id: "t1".into(),
                text: "hi".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .unwrap();
        assert!(out.replayed);
        assert_eq!(out.assistant_text, "hello from shell host");
        assert_eq!(host.model_stream_opens(), opens_before);
        let mut replayed = ConversationItem::assistant(out.assistant_text);
        host.stamp_assistant(&mut replayed);
        assert_eq!(
            serde_json::to_value(replayed).unwrap(),
            serde_json::to_value(ConversationItem::Assistant(assistant)).unwrap()
        );
        let captured = sink.0.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].tools, evidence.tools);
        assert_eq!(captured[0].system, evidence.system);
    }

    #[tokio::test]
    async fn replace_sampling_config_updates_model() {
        let dir = tempfile::tempdir().unwrap();
        let host = ShellHyperHost::new(test_sampler_config(), dir.path());
        assert_eq!(host.sampling_config().model, "test-model");
        let mut next = test_sampler_config();
        next.model = "other-model".into();
        host.replace_sampling_config(next);
        assert_eq!(host.sampling_config().model, "other-model");
    }

    #[tokio::test]
    async fn open_stream_rejects_empty_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_sampler_config();
        cfg.api_key = None;
        let host = ShellHyperHost::new(cfg, dir.path());
        let result = host
            .open_model_stream(ModelStreamRequest {
                session_id: "s".into(),
                turn_id: "t".into(),
                model: "m".into(),
                messages: vec![],
                tools: vec![],
                json_schema: None,
            })
            .await;
        let err = match result {
            Ok(_) => panic!("expected credential error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no api_key"), "unexpected: {err}");
        assert_eq!(host.model_stream_opens(), 0);
    }
}
