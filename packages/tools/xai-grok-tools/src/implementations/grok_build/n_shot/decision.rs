//! Record which n-shot result the user went with.
//!
//! The parent session applies the chosen changes with ordinary edits, so nothing
//! else can tell that a shot was selected. The model reports it here, and the
//! decision is appended to `decisions.jsonl` next to the run's `shots.json`.

use std::path::{Path, PathBuf};

const REPORT_FILE: &str = "report.md";
const MANIFEST_FILE: &str = "shots.json";
const DECISIONS_FILE: &str = "decisions.jsonl";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NShotOutcome {
    /// One shot was taken as it is.
    Single,
    /// Parts of several shots were combined.
    Hybrid,
    /// No shot was used.
    None,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct NShotResolve {
    #[schemars(
        description = "Path of the run's report.md, as given in the n-shot completion message."
    )]
    pub report_path: String,
    #[schemars(description = "`single`, `hybrid` (parts of several shots), or `none`.")]
    pub outcome: NShotOutcome,
    #[schemars(description = "Labels of the shots that were used. Empty for `none`.")]
    #[serde(default)]
    pub shots: Vec<String>,
    #[schemars(
        description = "For `hybrid`: the label of the shot the other parts were ported into."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[schemars(description = "One or two sentences: why this outcome.")]
    pub rationale: String,
}

/// Purpose: Check a decision against the labels of the run's shots.
/// Input: The decision and the shot labels from `shots.json`.
/// Output: `Ok` when the decision is consistent, or an error message for the model.
pub fn validate(resolve: &NShotResolve, labels: &[String]) -> Result<(), String> {
    if resolve.rationale.trim().is_empty() {
        return Err("`rationale` must not be empty".into());
    }
    if let Some(unknown) = resolve.shots.iter().find(|s| !labels.contains(s)) {
        return Err(format!(
            "unknown shot `{unknown}`; the shots of this run are: {}",
            labels.join(", ")
        ));
    }
    let count = resolve.shots.len();
    match resolve.outcome {
        NShotOutcome::Single if count != 1 => Err("`single` needs exactly one shot".into()),
        NShotOutcome::Hybrid if count < 2 => Err("`hybrid` needs at least two shots".into()),
        NShotOutcome::None if count != 0 => Err("`none` must not list shots".into()),
        _ => match &resolve.base {
            Some(base) if !resolve.shots.contains(base) => {
                Err(format!("`base` `{base}` must be one of `shots`"))
            }
            _ => Ok(()),
        },
    }
}

/// Purpose: Append the decision for the run that owns `report_path`.
/// Input: The decision, and the working directory where the changes were applied.
/// Output: A confirmation message, or an error message for the model.
pub async fn record(resolve: NShotResolve, applied_to: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || record_blocking(&resolve, &applied_to))
        .await
        .map_err(|e| format!("decision write was interrupted: {e}"))?
}

fn record_blocking(resolve: &NShotResolve, applied_to: &Path) -> Result<String, String> {
    let scratch = run_scratch_dir(Path::new(&resolve.report_path))?;
    let labels = read_labels(&scratch.join(MANIFEST_FILE))?;
    validate(resolve, &labels)?;
    let line = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "outcome": resolve.outcome,
        "shots": resolve.shots,
        "base": resolve.base,
        "rationale": resolve.rationale.trim(),
        "applied_to": applied_to,
    });
    append_line(&scratch.join(DECISIONS_FILE), &line.to_string())?;
    Ok(format!(
        "Recorded the n-shot outcome ({}) in {}.",
        serde_json::to_value(resolve.outcome)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default(),
        scratch.join(DECISIONS_FILE).display()
    ))
}

/// Return the directory of an n-shot run, from the path of its report.
/// Only `…/workflows/<run>/scratch/report.md` (subagent shots) and
/// `…/n-shot/<label>/report.md` (session shots) are accepted, so a wrong or
/// made-up path cannot make this tool write somewhere else.
fn run_scratch_dir(report_path: &Path) -> Result<PathBuf, String> {
    if !report_path.is_absolute() {
        return Err("report_path must be the absolute path of report.md".into());
    }
    let report = report_path
        .canonicalize()
        .map_err(|e| format!("cannot read report_path `{}`: {e}", report_path.display()))?;
    let name_of = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(String::from);
    let scratch = report.parent();
    let runs = scratch.and_then(Path::parent).and_then(Path::parent);
    let is_workflow_dir = scratch.and_then(name_of).as_deref() == Some("scratch")
        && runs.and_then(name_of).as_deref() == Some("workflows");
    let is_session_group_dir =
        scratch.and_then(Path::parent).and_then(name_of).as_deref() == Some("n-shot");
    let is_run_report = name_of(&report).as_deref() == Some(REPORT_FILE)
        && (is_workflow_dir || is_session_group_dir);
    match scratch {
        Some(dir) if is_run_report => Ok(dir.to_path_buf()),
        _ => {
            Err("report_path must be the report.md path from the n-shot completion message".into())
        }
    }
}

fn read_labels(manifest_path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{MANIFEST_FILE} is not valid JSON: {e}"))?;
    let shots = manifest["shots"]
        .as_array()
        .ok_or_else(|| format!("{MANIFEST_FILE} has no `shots` list"))?;
    Ok(shots
        .iter()
        .filter_map(|s| s["label"].as_str().map(String::from))
        .collect())
}

fn append_line(path: &Path, line: &str) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(outcome: NShotOutcome, shots: &[&str]) -> NShotResolve {
        NShotResolve {
            report_path: String::new(),
            outcome,
            shots: shots.iter().map(|s| s.to_string()).collect(),
            base: None,
            rationale: "smaller diff".into(),
        }
    }

    fn labels() -> Vec<String> {
        vec!["shot-1".into(), "shot-2".into()]
    }

    #[test]
    fn validates_each_outcome() {
        let ok = [
            resolve(NShotOutcome::Single, &["shot-2"]),
            resolve(NShotOutcome::Hybrid, &["shot-1", "shot-2"]),
            resolve(NShotOutcome::None, &[]),
        ];
        for r in &ok {
            assert_eq!(validate(r, &labels()), Ok(()), "{r:?}");
        }
        let bad = [
            resolve(NShotOutcome::Single, &["shot-1", "shot-2"]),
            resolve(NShotOutcome::Single, &["shot-9"]),
            resolve(NShotOutcome::Hybrid, &["shot-1"]),
            resolve(NShotOutcome::None, &["shot-1"]),
        ];
        for r in &bad {
            assert!(validate(r, &labels()).is_err(), "{r:?}");
        }
    }

    #[test]
    fn base_must_be_a_used_shot() {
        let mut r = resolve(NShotOutcome::Hybrid, &["shot-1", "shot-2"]);
        r.base = Some("shot-3".into());
        assert!(validate(&r, &labels()).is_err());
        r.base = Some("shot-2".into());
        assert_eq!(validate(&r, &labels()), Ok(()));
    }

    #[test]
    fn records_one_line_per_decision_and_rejects_foreign_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("workflows/wf_1/scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join(REPORT_FILE), "report").unwrap();
        std::fs::write(
            scratch.join(MANIFEST_FILE),
            r#"{"shots":[{"label":"shot-1"},{"label":"shot-2"}]}"#,
        )
        .unwrap();
        let mut r = resolve(NShotOutcome::Single, &["shot-2"]);
        r.report_path = scratch.join(REPORT_FILE).to_string_lossy().into_owned();
        record_blocking(&r, Path::new("/work")).unwrap();
        record_blocking(&r, Path::new("/work")).unwrap();
        let log = std::fs::read_to_string(scratch.join(DECISIONS_FILE)).unwrap();
        let first: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(log.lines().count(), 2);
        assert_eq!(first["outcome"], "single");
        assert_eq!(first["shots"][0], "shot-2");

        let group = tmp.path().canonicalize().unwrap().join("n-shot/n-shot-1");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(group.join(REPORT_FILE), "report").unwrap();
        std::fs::write(
            group.join(MANIFEST_FILE),
            r#"{"shots":[{"label":"shot-2"}]}"#,
        )
        .unwrap();
        r.report_path = group.join(REPORT_FILE).to_string_lossy().into_owned();
        record_blocking(&r, Path::new("/work")).unwrap();
        assert!(group.join(DECISIONS_FILE).exists());

        let elsewhere = tmp.path().join("report.md");
        std::fs::write(&elsewhere, "x").unwrap();
        r.report_path = elsewhere.to_string_lossy().into_owned();
        assert!(record_blocking(&r, Path::new("/work")).is_err());
    }
}
