//! `bladerunner sovereign` — models served from hardware you control.
//!
//! Purpose: Register a machine that serves a model, then start, stop and check it.
//! A registered sovereign also becomes a normal model named `<sovereign>/<slug>`.
//! Method: The registry is `sovereigns.toml` in the Bladerunner home directory.
//! `add` also writes a `[model."<sovereign>/<slug>"]` entry to `config.toml`.
//! `up` and `down` run the recipe scripts over ssh. `status` probes `/v1/models`.
//!
//! Protocol: Every recipe directory provides the same entry points, run on the host.
//! - `./start.sh` returns only when the model serves. `./stop.sh` stops it.
//! - The API is OpenAI-compatible on port 8888, and `GET /v1/models` lists the model.
//! - `./api-key.sh` is optional. It prints the API key, or the recipe has no key.
//! A preset holds only data: the directory, model id and context window.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

/// `[sovereign] show_commands = false` in config.toml hides `/sovereign` and `/tailnet`.
const SHOW_COMMANDS_KEY: &str = "show_commands";
const REGISTRY_FILE: &str = "sovereigns.toml";
const CONFIG_FILE: &str = "config.toml";
const PROBE_TIMEOUT_S: u64 = 4;
const POLL_INTERVAL_S: u64 = 5;
const START_SCRIPT: &str = "./start.sh";
const STOP_SCRIPT: &str = "./stop.sh";
const API_KEY_SCRIPT: &str = "./api-key.sh";
const PORT: u16 = 8888;
const READY_TIMEOUT_S: u64 = 900;
const SSH_CONNECT_TIMEOUT_S: u64 = 8;

/// A starred recipe. Hard-coded on purpose: these are the verified setups.
struct Preset {
    id: &'static str,
    slug: &'static str,
    model: &'static str,
    display: &'static str,
    dir: &'static str,
    context_window: u64,
}

const PRESETS: &[Preset] = &[
    Preset {
        id: "qwen38-flash-dual-spark",
        slug: "qwen-3.8-flash",
        model: "Qwen3.8-Flash-Next",
        display: "Qwen 3.8 Flash",
        dir: "~/ai/Qwen3.8-Flash-Dual-DGX-Sparks-TensorFold",
        context_window: 1_048_576,
    },
    // Apple-silicon Mac serving through oMLX.
    Preset {
        id: "qwen38-flash-mac-studio",
        slug: "qwen-3.8-flash",
        model: "Qwen3.8-Flash-Next-4e8d8g",
        display: "Qwen 3.8 Flash",
        dir: "~/ai/mac-serve",
        context_window: 32_768,
    },
];

#[derive(Debug, clap::Args, Clone)]
pub struct SovereignArgs {
    #[command(subcommand)]
    pub command: SovereignCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum SovereignCommand {
    /// List registered sovereigns and whether each one is serving
    List,
    /// Register a sovereign from a starred recipe
    Add {
        /// Name of the sovereign, for example `dgx-spark`
        name: String,
        /// Starred recipe id
        #[arg(long, default_value = "qwen38-flash-dual-spark")]
        preset: String,
        /// Where the recipe is installed, as `[user@]host` (tailnet name or IP)
        #[arg(long)]
        ssh: String,
        /// Override the endpoint, for example `http://host.example:8888/v1`
        #[arg(long)]
        base_url: Option<String>,
        /// API key of the server. Only for a recipe that does not publish its key file
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Show whether a sovereign is serving
    Status { name: Option<String> },
    /// Start a sovereign and wait until it serves
    Up { name: Option<String> },
    /// Stop a sovereign
    Down { name: Option<String> },
    /// Remove a sovereign from the registry and from the model list
    Remove { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Sovereign {
    ssh: String,
    dir: String,
    start: String,
    stop: String,
    base_url: String,
    model: String,
    slug: String,
    ready_timeout_s: u64,
    /// Sent as a bearer token. `up` fills it from `./api-key.sh` when the recipe has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    sovereign: BTreeMap<String, Sovereign>,
}

#[derive(Debug, PartialEq)]
enum Health {
    /// The endpoint answers and lists the model.
    Serving,
    /// The endpoint answers but does not list the model.
    WrongModel(Vec<String>),
    /// The endpoint answers but refuses the API key.
    Unauthorized,
    /// The endpoint does not answer.
    Down,
}

/// Whether `/sovereign` and `/tailnet` are listed in the slash menu. Read once per run.
///
/// The commands stay runnable when hidden. The default is to show them.
pub fn commands_listed() -> bool {
    static LISTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LISTED.get_or_init(|| {
        xai_grok_config::load_effective_config_disk_only()
            .map(|config| listed_in_config(&config))
            .unwrap_or(true)
    })
}

fn listed_in_config(config: &toml::Value) -> bool {
    config
        .get("sovereign")
        .and_then(|table| table.get(SHOW_COMMANDS_KEY))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// Work that the `/sovereign` and `/tailnet` slash commands run in the background.
#[derive(Debug, Clone, PartialEq)]
pub enum SovereignTask {
    List,
    Up(Option<String>),
    Down(Option<String>),
    Status(Option<String>),
    Tailnet,
}

impl SovereignTask {
    /// A short line to show when the task starts.
    pub fn start_label(&self) -> String {
        match self {
            Self::Up(name) => format!("Starting {} ...", name.as_deref().unwrap_or("sovereign")),
            Self::Down(name) => format!("Stopping {} ...", name.as_deref().unwrap_or("sovereign")),
            Self::List | Self::Status(_) | Self::Tailnet => "Checking ...".to_string(),
        }
    }
}

/// Runs one task without touching the terminal. A failure becomes text, not an error.
pub async fn run_task(task: SovereignTask) -> String {
    let home = xai_grok_config::grok_home();
    let result = match task {
        SovereignTask::List => list(&home).await,
        SovereignTask::Up(name) => up(&home, name.as_deref(), Output::Captured).await,
        SovereignTask::Down(name) => down(&home, name.as_deref(), Output::Captured).await,
        SovereignTask::Status(name) => status(&home, name.as_deref()).await,
        SovereignTask::Tailnet => crate::tailnet::report().await,
    };
    result.unwrap_or_else(|e| format!("{e:#}"))
}

pub async fn run(args: SovereignArgs) -> Result<()> {
    let home = xai_grok_config::grok_home();
    let report = match args.command {
        SovereignCommand::List => list(&home).await,
        SovereignCommand::Add {
            name,
            preset,
            ssh,
            base_url,
            api_key,
        } => {
            let allowed = effective_allowed_models()?;
            add(&home, &name, &preset, &ssh, base_url, api_key, allowed)
        }
        SovereignCommand::Status { name } => status(&home, name.as_deref()).await,
        SovereignCommand::Up { name } => up(&home, name.as_deref(), Output::Terminal).await,
        SovereignCommand::Down { name } => down(&home, name.as_deref(), Output::Terminal).await,
        SovereignCommand::Remove { name } => remove(&home, &name),
    }?;
    println!("{report}");
    Ok(())
}

/// Where the output of a remote script goes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Output {
    /// Stream it to the terminal, as the command line does.
    Terminal,
    /// Keep it out of the way, as the TUI needs.
    Captured,
}

async fn list(home: &Path) -> Result<String> {
    let registry = load_registry(home)?;
    if registry.sovereign.is_empty() {
        return Ok("No sovereigns. Add one with `sovereign add <name> --ssh <host>`.".into());
    }
    let mut lines = Vec::new();
    for (name, sovereign) in &registry.sovereign {
        let health = probe(sovereign).await;
        lines.push(describe(name, sovereign, &health));
    }
    Ok(lines.join("\n"))
}

fn add(
    home: &Path,
    name: &str,
    preset_id: &str,
    ssh: &str,
    base_url: Option<String>,
    api_key: Option<String>,
    allowed_models: Option<Vec<String>>,
) -> Result<String> {
    let preset = find_preset(preset_id)?;
    let sovereign = sovereign_from_preset(preset, ssh, base_url, api_key);
    let mut registry = load_registry(home)?;
    registry.sovereign.insert(name.to_string(), sovereign.clone());
    save_registry(home, &registry)?;
    upsert_model_entry(home, name, preset, &sovereign)?;
    // An empty allowlist already permits every model.
    if let Some(allowed) = allowed_models.filter(|allowed| !allowed.is_empty()) {
        set_allowed_models(home, with_pattern(allowed, &allow_pattern(name)))?;
    }
    Ok(format!(
        "Added {name}. Model: {}\nEndpoint: {}",
        model_key(name, &sovereign.slug),
        sovereign.base_url
    ))
}

async fn status(home: &Path, name: Option<&str>) -> Result<String> {
    let (name, sovereign) = pick(home, name)?;
    let health = probe(&sovereign).await;
    let mut report = describe(&name, &sovereign, &health);
    if health == Health::Down {
        let reachable = is_ssh_reachable(&sovereign.ssh).await;
        let host = if reachable { "reachable by ssh" } else { "NOT reachable by ssh" };
        report.push_str(&format!("\n  host {host}"));
    }
    Ok(report)
}

async fn up(home: &Path, name: Option<&str>, output: Output) -> Result<String> {
    let (name, sovereign) = pick(home, name)?;
    if probe(&sovereign).await == Health::Serving {
        return Ok(format!("{name} is already serving."));
    }
    if output == Output::Terminal {
        println!("Starting {name} on {} ...", sovereign.ssh);
    }
    run_remote(&sovereign, &sovereign.start, output).await?;
    let sovereign = refresh_api_key(home, &name, sovereign).await?;
    wait_until_serving(&name, &sovereign).await
}

async fn down(home: &Path, name: Option<&str>, output: Output) -> Result<String> {
    let (name, sovereign) = pick(home, name)?;
    if output == Output::Terminal {
        println!("Stopping {name} on {} ...", sovereign.ssh);
    }
    run_remote(&sovereign, &sovereign.stop, output).await?;
    match probe(&sovereign).await {
        Health::Down => Ok(format!("{name} is down.")),
        _ => bail!("{name} still answers after its stop script ran"),
    }
}

fn remove(home: &Path, name: &str) -> Result<String> {
    let mut registry = load_registry(home)?;
    let Some(sovereign) = registry.sovereign.remove(name) else {
        bail!("no sovereign named {name}");
    };
    save_registry(home, &registry)?;
    remove_model_entry(home, &model_key(name, &sovereign.slug))?;
    remove_allowed_pattern(home, &allow_pattern(name))?;
    Ok(format!("Removed {name}. The server and its files were not touched."))
}

// ---- pure helpers ----

fn find_preset(id: &str) -> Result<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id).with_context(|| {
        let ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        format!("unknown preset {id}. Starred presets: {}", ids.join(", "))
    })
}

fn sovereign_from_preset(
    preset: &Preset,
    ssh: &str,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Sovereign {
    Sovereign {
        ssh: ssh.to_string(),
        dir: preset.dir.to_string(),
        start: START_SCRIPT.to_string(),
        stop: STOP_SCRIPT.to_string(),
        base_url: base_url.unwrap_or_else(|| default_base_url(ssh, PORT)),
        model: preset.model.to_string(),
        slug: preset.slug.to_string(),
        ready_timeout_s: READY_TIMEOUT_S,
        api_key,
    }
}

/// `user@host` -> `http://host:<port>/v1`.
fn default_base_url(ssh: &str, port: u16) -> String {
    let host = ssh.rsplit('@').next().unwrap_or(ssh);
    format!("http://{host}:{port}/v1")
}

fn model_key(name: &str, slug: &str) -> String {
    format!("{name}/{slug}")
}

/// Bladerunner ships an `allowed_models` list, so each sovereign needs its own pattern.
fn allow_pattern(name: &str) -> String {
    format!("{name}/*")
}

fn with_pattern(mut allowed: Vec<String>, pattern: &str) -> Vec<String> {
    if !allowed.iter().any(|p| p == pattern) {
        allowed.push(pattern.to_string());
    }
    allowed
}

fn ssh_args(sovereign: &Sovereign, remote_command: &str) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_S}"),
        sovereign.ssh.clone(),
        // The dir may start with `~`, so it is left unquoted for the remote shell.
        format!("cd {} && {remote_command}", sovereign.dir),
    ]
}

fn parse_model_ids(body: &serde_json::Value) -> Vec<String> {
    body["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn health_from_ids(ids: Vec<String>, model: &str) -> Health {
    if ids.iter().any(|id| id == model) {
        Health::Serving
    } else {
        Health::WrongModel(ids)
    }
}

fn describe(name: &str, sovereign: &Sovereign, health: &Health) -> String {
    let state = match health {
        Health::Serving => "serving".to_string(),
        Health::WrongModel(ids) => format!("answering, but lists {ids:?} not {}", sovereign.model),
        Health::Unauthorized => "answering, but refuses the API key".to_string(),
        Health::Down => "down".to_string(),
    };
    format!(
        "{name}/{}  {state}  ({})",
        sovereign.slug, sovereign.base_url
    )
}

// ---- I/O ----

fn pick(home: &Path, name: Option<&str>) -> Result<(String, Sovereign)> {
    let mut registry = load_registry(home)?;
    match name {
        Some(name) => {
            let sovereign = registry
                .sovereign
                .remove(name)
                .with_context(|| format!("no sovereign named {name}"))?;
            Ok((name.to_string(), sovereign))
        }
        None if registry.sovereign.len() == 1 => {
            Ok(registry.sovereign.into_iter().next().expect("one entry"))
        }
        None => bail!("name a sovereign: {:?}", registry.sovereign.keys().collect::<Vec<_>>()),
    }
}

async fn probe(sovereign: &Sovereign) -> Health {
    let url = format!("{}/models", sovereign.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_S))
        .build()
        .expect("static client config");
    let mut request = client.get(&url);
    if let Some(key) = &sovereign.api_key {
        request = request.bearer_auth(key);
    }
    let Ok(response) = request.send().await else {
        return Health::Down;
    };
    if matches!(response.status().as_u16(), 401 | 403) {
        return Health::Unauthorized;
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return Health::Down;
    };
    health_from_ids(parse_model_ids(&body), &sovereign.model)
}

async fn is_ssh_reachable(ssh: &str) -> bool {
    tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes"])
        .args(["-o", &format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_S}")])
        .args([ssh, "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs a script on the sovereign host.
async fn run_remote(sovereign: &Sovereign, remote_command: &str, output: Output) -> Result<()> {
    let mut command = tokio::process::Command::new("ssh");
    command.args(ssh_args(sovereign, remote_command));
    if output == Output::Terminal {
        let status = command.status().await.context("could not run ssh")?;
        return check_exit(status, remote_command, &sovereign.ssh, "");
    }
    let result = command.output().await.context("could not run ssh")?;
    let tail = last_line(&result.stderr).or_else(|| last_line(&result.stdout));
    check_exit(result.status, remote_command, &sovereign.ssh, tail.as_deref().unwrap_or(""))
}

/// Copies the recipe's API key into the model entry and the registry.
async fn refresh_api_key(home: &Path, name: &str, mut sovereign: Sovereign) -> Result<Sovereign> {
    let Some(key) = read_remote_key(&sovereign).await? else {
        return Ok(sovereign);
    };
    sovereign.api_key = Some(key);
    // Always store, so a model entry that drifted from the registry is repaired.
    store_api_key(home, name, &sovereign)?;
    Ok(sovereign)
}

/// Runs `./api-key.sh`. A recipe without that script has no key, which is not an error.
async fn read_remote_key(sovereign: &Sovereign) -> Result<Option<String>> {
    let script = format!("test -x {API_KEY_SCRIPT} || exit 0; {API_KEY_SCRIPT}");
    let output = tokio::process::Command::new("ssh")
        .args(ssh_args(sovereign, &script))
        .output()
        .await
        .context("could not run ssh")?;
    if !output.status.success() {
        bail!("{API_KEY_SCRIPT} failed on {}. Is the server set up?", sovereign.ssh);
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if key.chars().any(char::is_whitespace) {
        bail!("{API_KEY_SCRIPT} on {} did not print a usable API key", sovereign.ssh);
    }
    Ok((!key.is_empty()).then_some(key))
}

fn check_exit(
    status: std::process::ExitStatus,
    remote_command: &str,
    ssh: &str,
    detail: &str,
) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    bail!("`{remote_command}` failed on {ssh} ({status}). {detail}")
}

fn last_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string())
}

async fn wait_until_serving(name: &str, sovereign: &Sovereign) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(sovereign.ready_timeout_s);
    while Instant::now() < deadline {
        if probe(sovereign).await == Health::Serving {
            return Ok(format!("{name} is serving at {}", sovereign.base_url));
        }
        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_S)).await;
    }
    bail!(
        "{name} did not serve within {} s. Check the logs on {}.",
        sovereign.ready_timeout_s,
        sovereign.ssh
    )
}

fn registry_path(home: &Path) -> PathBuf {
    home.join(REGISTRY_FILE)
}

fn load_registry(home: &Path) -> Result<Registry> {
    let path = registry_path(home);
    if !path.exists() {
        return Ok(Registry::default());
    }
    let text = std::fs::read_to_string(&path)?;
    toml_edit::de::from_str(&text).with_context(|| format!("invalid {}", path.display()))
}

fn save_registry(home: &Path, registry: &Registry) -> Result<()> {
    let text = toml_edit::ser::to_string_pretty(registry)?;
    // The registry can hold an API key.
    write_private(&registry_path(home), &text)
}

/// Writes a file that only the owner can read, before any secret reaches the disk.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` applies only to a new file, so an existing one is restricted here.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Copies the key into the model entry first, then the registry.
///
/// If the config write fails, the registry still lacks the key, so the next `up` fetches it again.
fn store_api_key(home: &Path, name: &str, sovereign: &Sovereign) -> Result<()> {
    let key = sovereign.api_key.as_deref().context("no API key to store")?;
    let mut doc = load_config_doc(home)?;
    let entry = doc
        .get_mut("model")
        .and_then(|models| models.get_mut(&model_key(name, &sovereign.slug)))
        .and_then(|entry| entry.as_table_mut())
        .with_context(|| format!("no model entry for {name} in config.toml"))?;
    entry["api_key"] = toml_edit::value(key);
    write_private(&home.join(CONFIG_FILE), &doc.to_string())?;
    let mut registry = load_registry(home)?;
    registry.sovereign.insert(name.to_string(), sovereign.clone());
    save_registry(home, &registry)
}

fn load_config_doc(home: &Path) -> Result<toml_edit::DocumentMut> {
    let path = home.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    text.parse().with_context(|| format!("invalid {}", path.display()))
}

fn upsert_model_entry(home: &Path, name: &str, preset: &Preset, sovereign: &Sovereign) -> Result<()> {
    let mut doc = load_config_doc(home)?;
    let mut entry = toml_edit::Table::new();
    entry["model"] = toml_edit::value(sovereign.model.as_str());
    entry["name"] = toml_edit::value(format!("{} ({name})", preset.display));
    entry["base_url"] = toml_edit::value(sovereign.base_url.as_str());
    // Without a server key, a non-secret placeholder prevents fallback to cloud credentials.
    entry["api_key"] = toml_edit::value(sovereign.api_key.as_deref().unwrap_or("sovereign"));
    entry["context_window"] = toml_edit::value(preset.context_window as i64);
    let models = doc
        .entry("model")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_mut()
        .context("`model` in config.toml is not a table")?;
    models.insert(&model_key(name, &sovereign.slug), toml_edit::Item::Table(entry));
    let path = home.join(CONFIG_FILE);
    if sovereign.api_key.is_some() {
        return write_private(&path, &doc.to_string());
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// The merged `[models] allowed_models`, or `None` when nothing restricts models.
fn effective_allowed_models() -> Result<Option<Vec<String>>> {
    let config = xai_grok_config::load_effective_config_disk_only()?;
    let list = config.get("models").and_then(|m| m.get("allowed_models"));
    Ok(list.and_then(|v| v.as_array()).map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect()
    }))
}

fn set_allowed_models(home: &Path, allowed: Vec<String>) -> Result<()> {
    let mut doc = load_config_doc(home)?;
    let models = doc
        .entry("models")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .context("`models` in config.toml is not a table")?;
    models["allowed_models"] = toml_edit::value(allowed.into_iter().collect::<toml_edit::Array>());
    std::fs::write(home.join(CONFIG_FILE), doc.to_string())?;
    Ok(())
}

fn remove_allowed_pattern(home: &Path, pattern: &str) -> Result<()> {
    let mut doc = load_config_doc(home)?;
    let Some(list) = doc
        .get_mut("models")
        .and_then(|m| m.get_mut("allowed_models"))
        .and_then(|v| v.as_array_mut())
    else {
        return Ok(());
    };
    list.retain(|item| item.as_str() != Some(pattern));
    std::fs::write(home.join(CONFIG_FILE), doc.to_string())?;
    Ok(())
}

fn remove_model_entry(home: &Path, key: &str) -> Result<()> {
    let mut doc = load_config_doc(home)?;
    if let Some(models) = doc.get_mut("model").and_then(|m| m.as_table_mut()) {
        models.remove(key);
    }
    std::fs::write(home.join(CONFIG_FILE), doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Sovereign {
        sovereign_from_preset(&PRESETS[0], "user@dgx.example.ts.net", None, None)
    }

    #[test]
    fn commands_are_listed_unless_the_config_hides_them() {
        let parse = |text: &str| toml::from_str::<toml::Value>(text).unwrap();
        assert!(listed_in_config(&parse("")));
        assert!(listed_in_config(&parse("[sovereign]\nshow_commands = true")));
        assert!(!listed_in_config(&parse("[sovereign]\nshow_commands = false")));
    }

    #[test]
    fn default_base_url_drops_the_user() {
        assert_eq!(
            default_base_url("user@dgx.example.ts.net", 8888),
            "http://dgx.example.ts.net:8888/v1"
        );
        assert_eq!(default_base_url("10.0.0.5", 8888), "http://10.0.0.5:8888/v1");
    }

    #[test]
    fn ssh_args_leave_tilde_unquoted_and_never_prompt() {
        let args = ssh_args(&sample(), "./start.sh");
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert_eq!(
            args.last().unwrap(),
            "cd ~/ai/Qwen3.8-Flash-Dual-DGX-Sparks-TensorFold && ./start.sh"
        );
    }

    #[test]
    fn health_requires_the_registered_model() {
        let body = serde_json::json!({"data": [{"id": "Qwen3.8-Flash-Next"}]});
        assert_eq!(
            health_from_ids(parse_model_ids(&body), "Qwen3.8-Flash-Next"),
            Health::Serving
        );
        assert_eq!(
            health_from_ids(parse_model_ids(&body), "other"),
            Health::WrongModel(vec!["Qwen3.8-Flash-Next".into()])
        );
        assert!(parse_model_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn add_preserves_unrestricted_model_allowlists() {
        for allowed in [None, Some(Vec::new())] {
            let dir = tempfile::tempdir().unwrap();
            let original = if allowed.is_some() {
                "[models]\nallowed_models = []\n"
            } else {
                "[ui]\nyolo = false\n"
            };
            std::fs::write(dir.path().join(CONFIG_FILE), original).unwrap();
            add(
                dir.path(),
                "spark",
                "qwen38-flash-dual-spark",
                "u@spark",
                None,
                None,
                allowed.clone(),
            )
            .unwrap();
            let config = load_config_doc(dir.path()).unwrap();
            let actual = config
                .get("models")
                .and_then(|models| models.get("allowed_models"))
                .map(|value| value.as_array().unwrap().len());
            assert_eq!(actual, allowed.map(|items| items.len()));
        }
    }

    #[test]
    fn add_isolates_the_model_from_global_credentials() {
        let dir = tempfile::tempdir().unwrap();
        add(
            dir.path(),
            "spark",
            "qwen38-flash-dual-spark",
            "u@spark",
            None,
            None,
            None,
        )
        .unwrap();
        let config = load_config_doc(dir.path()).unwrap();
        let entry = &config["model"]["spark/qwen-3.8-flash"];
        assert_eq!(entry["api_key"].as_str(), Some("sovereign"));
        assert_eq!(entry["base_url"].as_str(), Some("http://spark:8888/v1"));
    }

    #[test]
    fn mac_preset_follows_the_protocol_and_stores_a_fetched_key() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "studio", "qwen38-flash-mac-studio", "u@studio", None, None, None).unwrap();
        let mut registry = load_registry(dir.path()).unwrap();
        let mut sovereign = registry.sovereign.remove("studio").unwrap();
        assert_eq!(sovereign.base_url, "http://studio:8888/v1");
        assert_eq!((sovereign.start.as_str(), sovereign.stop.as_str()), ("./start.sh", "./stop.sh"));
        assert_eq!(sovereign.api_key, None);

        sovereign.api_key = Some("mac-secret".into());
        store_api_key(dir.path(), "studio", &sovereign).unwrap();
        let config = load_config_doc(dir.path()).unwrap();
        assert_eq!(
            config["model"]["studio/qwen-3.8-flash"]["api_key"].as_str(),
            Some("mac-secret")
        );
        let stored = load_registry(dir.path()).unwrap();
        assert_eq!(stored.sovereign["studio"].api_key.as_deref(), Some("mac-secret"));
    }

    #[test]
    fn files_that_hold_a_key_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(CONFIG_FILE);
        std::fs::write(&config, "[ui]\nyolo = false\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        add(dir.path(), "box", "qwen38-flash-mac-studio", "u@box", None, Some("k".into()), None).unwrap();
        assert_eq!(mode(&config), 0o600);
        assert_eq!(mode(&registry_path(dir.path())), 0o600);

        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut sovereign = load_registry(dir.path()).unwrap().sovereign.remove("box").unwrap();
        sovereign.api_key = Some("k2".into());
        store_api_key(dir.path(), "box", &sovereign).unwrap();
        assert_eq!(mode(&config), 0o600);
    }

    #[test]
    fn a_failed_config_write_leaves_the_registry_key_unset() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "box", "qwen38-flash-mac-studio", "u@box", None, None, None).unwrap();
        let mut sovereign = load_registry(dir.path()).unwrap().sovereign.remove("box").unwrap();
        // The model entry is missing, as when config.toml was edited or restored.
        std::fs::write(dir.path().join(CONFIG_FILE), "").unwrap();
        sovereign.api_key = Some("k".into());
        assert!(store_api_key(dir.path(), "box", &sovereign).is_err());
        assert_eq!(load_registry(dir.path()).unwrap().sovereign["box"].api_key, None);
    }

    #[test]
    fn an_explicit_api_key_replaces_the_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        add(
            dir.path(),
            "box",
            "qwen38-flash-dual-spark",
            "u@box",
            None,
            Some("k1".into()),
            None,
        )
        .unwrap();
        let config = load_config_doc(dir.path()).unwrap();
        assert_eq!(config["model"]["box/qwen-3.8-flash"]["api_key"].as_str(), Some("k1"));
    }

    #[tokio::test]
    async fn list_handles_empty_and_multiple_sovereigns() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        assert!(
            list(dir.path())
                .await
                .unwrap()
                .starts_with("No sovereigns.")
        );
        let mut registry = Registry::default();
        for name in ["spark-a", "spark-b"] {
            let mut sovereign = sample();
            // An unsupported scheme fails without any network access.
            sovereign.base_url = "invalid://local/v1".into();
            registry.sovereign.insert(name.into(), sovereign);
        }
        save_registry(dir.path(), &registry).unwrap();
        let report = list(dir.path()).await.unwrap();
        assert_eq!(report.lines().count(), 2);
        assert!(report.contains("spark-a/qwen-3.8-flash  down"));
        assert!(report.contains("spark-b/qwen-3.8-flash  down"));
    }

    #[test]
    fn add_writes_registry_and_model_entry_then_remove_cleans_up() {
        let dir = std::env::temp_dir().join(format!("sov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), "[ui]\nyolo = false\n").unwrap();

        let bundled = Some(vec!["openai-codex/*".to_string()]);
        add(&dir, "dgx-spark", "qwen38-flash-dual-spark", "u@spark", None, None, bundled).unwrap();
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert!(config.contains("[ui]"), "existing config is kept");
        assert!(config.contains("[model.\"dgx-spark/qwen-3.8-flash\"]"));
        assert!(config.contains("base_url = \"http://spark:8888/v1\""));
        assert!(config.contains("\"openai-codex/*\", \"dgx-spark/*\""));
        let registry = load_registry(&dir).unwrap();
        assert_eq!(registry.sovereign["dgx-spark"].model, "Qwen3.8-Flash-Next");

        remove(&dir, "dgx-spark").unwrap();
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert!(!config.contains("dgx-spark"));
        assert!(config.contains("openai-codex/*"), "bundled pattern is kept");
        assert!(load_registry(&dir).unwrap().sovereign.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
