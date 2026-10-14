//! Versioned writeback record for exact model history and immutable request evidence.
use crate::session::persistence::{CopiedSessionFile, validate_snapshot_request_evidence};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, path::Path};
use xai_grok_sampling_types::ConversationItem;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestProvenance {
    pub schema_version: u32,
    pub chat_history: Vec<serde_json::Value>,
    pub files: BTreeMap<String, String>,
}

impl RequestProvenance {
    pub(crate) fn capture(dir: &Path, history: &[ConversationItem]) -> io::Result<Self> {
        let mut files = BTreeMap::new();
        for directory in ["request_tools", "system_prompts", "compaction_checkpoints"] {
            let entries = match std::fs::read_dir(dir.join(directory)) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let name = format!(
                        "{directory}/{}",
                        entry.file_name().to_str().ok_or_else(|| io::Error::new(
                            io::ErrorKind::InvalidData,
                            "non-UTF8 evidence filename"
                        ))?
                    );
                    files.insert(name, std::fs::read_to_string(entry.path())?);
                }
            }
        }
        let record = Self {
            schema_version: 1,
            chat_history: history
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()
                .map_err(io::Error::other)?,
            files,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.schema_version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported request provenance version",
            ));
        }
        let mut files = Vec::new();
        for (name, text) in &self.files {
            let path = Path::new(name);
            let parent = path.parent().and_then(Path::to_str);
            let extension = path.extension().and_then(|x| x.to_str());
            if !matches!(
                (parent, extension),
                (Some("request_tools"), Some("json"))
                    | (Some("system_prompts"), Some("txt"))
                    | (Some("compaction_checkpoints"), Some("json"))
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid request evidence path",
                ));
            }
            files.push(CopiedSessionFile {
                name: name.clone(),
                data: text.as_bytes().to_vec(),
            });
        }
        let mut chat = Vec::new();
        for item in &self.chat_history {
            // Check typed history as well as hashes before replacing any local cache.
            serde_json::from_value::<ConversationItem>(item.clone()).map_err(io::Error::other)?;
            serde_json::to_writer(&mut chat, item).map_err(io::Error::other)?;
            chat.push(b'\n');
        }
        files.push(CopiedSessionFile {
            name: "chat_history.jsonl".into(),
            data: chat,
        });
        validate_snapshot_request_evidence(&files).map_err(io::Error::other)
    }

    pub(crate) fn restore_evidence(&self, dir: &Path) -> io::Result<()> {
        self.validate()?;
        for (name, text) in &self.files {
            let path = dir.join(name);
            crate::util::grok_home::create_dir_all_owner_only(path.parent().unwrap())?;
            crate::session::storage::write_bytes_atomic(&path, text.as_bytes())?;
        }
        Ok(())
    }
}

/// A bounded transport fragment. Backend messages remain opaque JSON-RPC records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestProvenanceChunk {
    pub record_sha256: String,
    pub index: usize,
    pub total: usize,
    pub data: String,
}

#[derive(Serialize, Deserialize)]
struct Delta {
    base_record_sha256: Option<String>,
    keep_prefix: usize,
    append: Vec<serde_json::Value>,
    files: BTreeMap<String, String>,
    removed_files: Vec<String>,
    #[serde(default)]
    delivery_id: Option<String>,
}

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
fn record_digest(record: &RequestProvenance) -> io::Result<String> {
    Ok(digest(
        &serde_json::to_vec(record).map_err(io::Error::other)?,
    ))
}

/// Sends unchanged chat rows and immutable sidecars only once per writeback connection.
#[derive(Clone, Default)]
pub(crate) struct ProvenanceEncoder {
    previous: Option<RequestProvenance>,
}
impl ProvenanceEncoder {
    pub(crate) fn is_initialized(&self) -> bool {
        self.previous.is_some()
    }
    pub(crate) fn reset(&mut self) {
        self.previous = None;
    }
    pub(crate) fn encode(
        &mut self,
        record: RequestProvenance,
    ) -> io::Result<Vec<RequestProvenanceChunk>> {
        use base64::Engine;
        record.validate()?;
        let keep_prefix = self.previous.as_ref().map_or(0, |old| {
            old.chat_history
                .iter()
                .zip(&record.chat_history)
                .take_while(|(a, b)| a == b)
                .count()
        });
        let files: BTreeMap<_, _> = record
            .files
            .iter()
            .filter(|(name, text)| {
                self.previous.as_ref().and_then(|old| old.files.get(*name)) != Some(*text)
            })
            .map(|(name, text)| (name.clone(), text.clone()))
            .collect();
        let removed_files = self.previous.as_ref().map_or_else(Vec::new, |old| {
            old.files
                .keys()
                .filter(|name| !record.files.contains_key(*name))
                .cloned()
                .collect()
        });
        if self.previous.is_some()
            && keep_prefix == record.chat_history.len()
            && self.previous.as_ref().unwrap().chat_history.len() == keep_prefix
            && files.is_empty()
            && removed_files.is_empty()
        {
            return Ok(Vec::new());
        }
        let delta = Delta {
            base_record_sha256: self
                .previous
                .as_ref()
                .map(|old| record_digest(old))
                .transpose()?,
            keep_prefix,
            append: record.chat_history[keep_prefix..].to_vec(),
            files,
            removed_files,
            delivery_id: Some(uuid::Uuid::now_v7().to_string()),
        };
        let bytes = serde_json::to_vec(&delta).map_err(io::Error::other)?;
        let record_sha256 = digest(&bytes);
        // Including base64 + JSON escaping overhead, each message remains under 12 KiB.
        const CHUNK_BYTES: usize = 8 * 1024;
        let total = bytes.len().div_ceil(CHUNK_BYTES);
        let chunks = bytes
            .chunks(CHUNK_BYTES)
            .enumerate()
            .map(|(index, bytes)| RequestProvenanceChunk {
                record_sha256: record_sha256.clone(),
                index,
                total,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .collect();
        self.previous = Some(record);
        Ok(chunks)
    }
}

#[derive(Default)]
pub(crate) struct ProvenanceDecoder {
    previous: Option<RequestProvenance>,
    active: Option<(String, usize, Vec<Vec<u8>>)>,
    applied: std::collections::HashSet<String>,
}
impl ProvenanceDecoder {
    pub(crate) fn snapshot(&mut self, record: &RequestProvenance) -> io::Result<()> {
        record.validate()?;
        self.previous = Some(record.clone());
        self.active = None;
        self.applied.clear();
        Ok(())
    }
    pub(crate) fn chunk(
        &mut self,
        chunk: &RequestProvenanceChunk,
    ) -> io::Result<Option<RequestProvenance>> {
        use base64::Engine;
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete or corrupt request provenance delta",
            )
        };
        if chunk.total == 0 || chunk.index >= chunk.total {
            return Err(invalid());
        }
        // A lost POST response can replay complete records or any middle batch of
        // fragments. Neither must advance state twice or invalidate later deltas.
        if self.applied.contains(&chunk.record_sha256) {
            return Ok(None);
        }
        let piece = base64::engine::general_purpose::STANDARD
            .decode(&chunk.data)
            .map_err(io::Error::other)?;
        if chunk.index == 0
            && self
                .active
                .as_ref()
                .is_none_or(|(hash, _, _)| hash != &chunk.record_sha256)
        {
            self.active = Some((chunk.record_sha256.clone(), chunk.total, Vec::new()));
        }
        let (hash, total, pieces) = self.active.as_mut().ok_or_else(invalid)?;
        if hash != &chunk.record_sha256 || *total != chunk.total {
            return Err(invalid());
        }
        if chunk.index < pieces.len() {
            return if pieces[chunk.index] == piece {
                Ok(None)
            } else {
                Err(invalid())
            };
        }
        if chunk.index != pieces.len() {
            return Err(invalid());
        }
        pieces.push(piece);
        if pieces.len() != *total {
            return Ok(None);
        }
        let (hash, _, pieces) = self.active.take().unwrap();
        let bytes: Vec<u8> = pieces.into_iter().flatten().collect();
        if digest(&bytes) != hash {
            return Err(invalid());
        }
        let delta: Delta = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        let mut record = if let Some(base) = delta.base_record_sha256 {
            let old = self.previous.as_ref().ok_or_else(invalid)?;
            if record_digest(old)? != base {
                return Err(invalid());
            }
            old.clone()
        } else {
            if delta.keep_prefix != 0 {
                return Err(invalid());
            }
            RequestProvenance {
                schema_version: 1,
                chat_history: Vec::new(),
                files: BTreeMap::new(),
            }
        };
        if delta.keep_prefix > record.chat_history.len() {
            return Err(invalid());
        }
        record.chat_history.truncate(delta.keep_prefix);
        record.chat_history.extend(delta.append);
        for name in delta.removed_files {
            record.files.remove(&name);
        }
        record.files.extend(delta.files);
        record.validate()?;
        self.previous = Some(record.clone());
        self.applied.insert(hash);
        Ok(Some(record))
    }
    pub(crate) fn discard_corrupt(&mut self) {
        self.active = None;
        self.previous = None;
        self.applied.clear();
    }
    pub(crate) fn finish(&self) -> io::Result<()> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unfinished request provenance record",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(history: &[ConversationItem]) -> RequestProvenance {
        RequestProvenance {
            schema_version: 1,
            chat_history: history
                .iter()
                .map(|item| serde_json::to_value(item).unwrap())
                .collect(),
            files: BTreeMap::new(),
        }
    }

    #[test]
    fn deltas_are_linear_and_round_trip_history_replacements() {
        let mut encoder = ProvenanceEncoder::default();
        let mut decoder = ProvenanceDecoder::default();
        let mut history = Vec::new();
        let mut wire_bytes = 0;
        for n in 0..100 {
            history.push(ConversationItem::user(format!("{n}:{}", "x".repeat(1024))));
            let expected = record(&history);
            let chunks = encoder.encode(expected.clone()).unwrap();
            for chunk in chunks {
                wire_bytes += chunk.data.len();
                if let Some(actual) = decoder.chunk(&chunk).unwrap() {
                    assert_eq!(actual, expected);
                }
            }
            assert!(encoder.encode(expected).unwrap().is_empty());
        }
        assert!(
            wire_bytes < 200_000,
            "append-only history must have linear transfer cost"
        );
        let expected = record(&[
            ConversationItem::system("compacted"),
            ConversationItem::user("next"),
        ]);
        for chunk in encoder.encode(expected.clone()).unwrap() {
            if let Some(actual) = decoder.chunk(&chunk).unwrap() {
                assert_eq!(actual, expected);
            }
        }
        decoder.finish().unwrap();
    }

    #[test]
    fn large_unicode_evidence_is_chunked_once_and_integrity_checked() {
        let evidence =
            xai_grok_sampling_types::RequestEvidence::new(&[], Some("世界".repeat(20_000)));
        let mut assistant = match ConversationItem::assistant("done") {
            ConversationItem::Assistant(a) => a,
            _ => unreachable!(),
        };
        assistant.record_request(
            Some(&evidence.tools.0),
            evidence.system.as_ref().map(|s| s.0.as_str()),
            None,
        );
        let mut expected = record(&[ConversationItem::Assistant(assistant)]);
        expected.files.insert(
            format!("request_tools/{}.json", evidence.tools.0),
            String::from_utf8(evidence.tools.1).unwrap(),
        );
        let (hash, text) = evidence.system.unwrap();
        expected
            .files
            .insert(format!("system_prompts/{hash}.txt"), text);
        let mut encoder = ProvenanceEncoder::default();
        let chunks = encoder.encode(expected.clone()).unwrap();
        assert!(chunks.len() > 10);
        assert!(
            chunks
                .iter()
                .all(|c| serde_json::to_vec(c).unwrap().len() < 12 * 1024)
        );
        let mut decoder = ProvenanceDecoder::default();
        for chunk in &chunks {
            if let Some(actual) = decoder.chunk(chunk).unwrap() {
                assert_eq!(actual, expected);
            }
        }
        assert!(encoder.encode(expected.clone()).unwrap().is_empty());
        expected
            .chat_history
            .push(serde_json::to_value(ConversationItem::user("later")).unwrap());
        let next = encoder.encode(expected).unwrap();
        assert!(
            next.iter().map(|c| c.data.len()).sum::<usize>() < 1024,
            "immutable evidence must not be retransmitted"
        );
        let mut missing = ProvenanceDecoder::default();
        assert!(missing.chunk(&chunks[1]).is_err());
        missing.chunk(&chunks[0]).unwrap();
        assert!(missing.finish().is_err());
        let mut corrupt = chunks.clone();
        corrupt[0].data.replace_range(0..1, "A");
        let mut decoder = ProvenanceDecoder::default();
        assert!(corrupt.iter().any(|c| decoder.chunk(c).is_err()));
    }

    #[test]
    fn retried_records_and_partial_batches_are_idempotent() {
        let mut encoder = ProvenanceEncoder::default();
        let mut decoder = ProvenanceDecoder::default();
        let initial = record(&[ConversationItem::user("initial")]);
        let baseline = encoder.encode(initial.clone()).unwrap();
        for chunk in &baseline {
            decoder.chunk(chunk).unwrap();
        }
        let next = record(&[
            ConversationItem::user("initial"),
            ConversationItem::assistant("世界".repeat(20_000)),
        ]);
        let chunks = encoder.encode(next.clone()).unwrap();
        assert!(chunks.len() > 10);
        let mut completed = 0;
        // Simulate committed POST batches whose responses were lost, including
        // batches beginning in the middle of an unfinished record.
        for batch in chunks.chunks(5) {
            for _ in 0..2 {
                for chunk in batch {
                    if let Some(actual) = decoder.chunk(chunk).unwrap() {
                        assert_eq!(actual, next);
                        completed += 1;
                    }
                }
            }
        }
        assert_eq!(completed, 1);
        // Older duplicate baseline and non-baseline records must not roll back.
        for chunk in baseline.iter().chain(&chunks) {
            assert!(decoder.chunk(chunk).unwrap().is_none());
        }
        let latest = record(&[ConversationItem::user("last")]);
        for chunk in encoder.encode(latest.clone()).unwrap() {
            if let Some(actual) = decoder.chunk(&chunk).unwrap() {
                assert_eq!(actual, latest);
            }
        }
        // A new independent baseline restoring an old state is a distinct delivery.
        encoder.reset();
        let reset = encoder.encode(initial.clone()).unwrap();
        assert_ne!(reset[0].record_sha256, baseline[0].record_sha256);
        assert_eq!(decoder.chunk(&reset[0]).unwrap(), Some(initial));
        decoder.finish().unwrap();
    }

    #[test]
    fn invalid_paths_versions_and_missing_evidence_fail_before_restoration() {
        let mut record = RequestProvenance {
            schema_version: 1,
            chat_history: vec![
                serde_json::json!({"type":"assistant","content":"done", "tools_sha256":"a".repeat(64)}),
            ],
            files: BTreeMap::new(),
        };
        let target = tempfile::tempdir().unwrap();
        assert!(record.restore_evidence(target.path()).is_err());
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
        record.chat_history.clear();
        record
            .files
            .insert("request_tools/../../escape.json".into(), "payload".into());
        assert!(record.restore_evidence(target.path()).is_err());
        record.files.clear();
        record.schema_version = 2;
        assert!(record.restore_evidence(target.path()).is_err());
    }
}
