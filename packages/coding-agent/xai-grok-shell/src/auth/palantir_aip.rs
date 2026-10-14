//! Palantir AIP (Foundry) sign-in and the credential helper behind the
//! bundled `[auth_provider.palantir-aip]`.
//!
//! The `[palantir_aip]` config table names the Foundry hostname and the OAuth
//! client registered on it. Sign-in is OAuth2 authorization code + PKCE
//! against the hostname's Multipass endpoints, as a public client (no secret).
//! Model requests never read the credential file directly: the auth provider
//! runs `bladerunner auth palantir-aip-token`, which prints a fresh access
//! token and owns refresh. Foundry rotates refresh tokens on every use and
//! rejects reuse, so refresh happens under a cross-process file lock.
//!
//! A Foundry personal token is the other way in: pasted at sign-in (saved as
//! `foundry-aip.token`) or exported as `FOUNDRY_TOKEN`, Palantir's SDK
//! convention. It needs no OAuth client, so it works before an admin has
//! registered the Bladerunner app. Bladerunner ships no Foundry hostname or
//! client ID: they come from config or `FOUNDRY_HOSTNAME`.

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
};
use tokio::net::TcpListener;

use crate::auth::flow::{AuthChannels, AuthUrlInfo, AuthUrlMode};
use crate::auth::openai_codex::{
    callback_page, create_state, generate_pkce, parse_authorization_input,
};

/// The `[model_providers.*]` and `[auth_provider.*]` id used by the bundled config.
pub const PROVIDER_ID: &str = "palantir-aip";
/// Language-model proxy calls plus a refresh token; nothing else in Foundry.
pub const OAUTH_SCOPE: &str = "api:use-language-models-execute offline_access";

const AUTHORIZE_PATH: &str = "/multipass/api/oauth2/authorize";
const TOKEN_PATH: &str = "/multipass/api/oauth2/token";
/// Foundry-hosted page that displays the authorization code for pasting.
const HOSTED_CALLBACK_PATH: &str = "/multipass/api/oauth2/callback";
const ANTHROPIC_PROXY_PATH: &str = "/api/v2/llm/proxy/anthropic/v1";

const OAUTH_CREDENTIAL_FILE: &str = "foundry-aip-oauth.json";
const TOKEN_FILE: &str = "foundry-aip.token";
/// Refresh when the access token has less than this left; covers the auth
/// provider's own 60s skew plus a slow turn.
const REFRESH_MARGIN_SECS: i64 = 300;
/// A token that can't be refreshed is served until this close to expiry
/// (just above the auth provider's 60s skew), however short-lived it was.
const NO_REFRESH_MARGIN_SECS: i64 = 90;
/// Longest the agent caches a helper token before running the helper again.
/// The helper is the only place that sees credential files, so this bounds
/// how long a logout or a replaced token from another process goes unnoticed.
const RECHECK_SECS: u64 = 300;
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const LOGIN_HINT: &str = "Run `bladerunner login --palantir-aip` or `/login palantir-aip`.";
const SUPERSEDED: &str =
    "Palantir AIP sign-in was cancelled or replaced by a newer one; nothing was saved.";
/// Palantir SDK environment variables, honored when config sets nothing.
pub const HOSTNAME_ENV: &str = "FOUNDRY_HOSTNAME";
pub const TOKEN_ENV: &str = "FOUNDRY_TOKEN";
/// Where a user creates a personal token (Account > Settings > Tokens).
const TOKEN_SETTINGS_PATH: &str = "/workspace/settings/tokens";
/// The Developer Console checklist for registering the Bladerunner app.
pub const APP_SETUP_DOC: &str = "docs/foundry-app.md";

// =============================================================================
// Config
// =============================================================================

/// The `[palantir_aip]` table.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct PalantirAipConfig {
    /// Foundry hostname, e.g. `acme.palantirfoundry.com`.
    pub hostname: Option<String>,
    /// OAuth client ID registered on that hostname (public client).
    pub client_id: Option<String>,
}

impl PalantirAipConfig {
    /// Read `[palantir_aip]` from a merged config, falling back to
    /// `FOUNDRY_HOSTNAME` for the hostname. A malformed table reads as empty.
    pub fn from_raw(raw: &toml::Value) -> Self {
        Self::from_raw_with_env(raw, std::env::var(HOSTNAME_ENV).ok())
    }

    fn from_raw_with_env(raw: &toml::Value, env_hostname: Option<String>) -> Self {
        let mut config: Self = raw
            .get("palantir_aip")
            .and_then(|v| v.clone().try_into().ok())
            .unwrap_or_default();
        let unset = |v: &Option<String>| v.as_deref().is_none_or(|v| v.trim().is_empty());
        if unset(&config.hostname) {
            config.hostname = env_hostname.filter(|h| !h.trim().is_empty());
        }
        config
    }

    /// Read `[palantir_aip]` from the merged on-disk config layers.
    pub fn load() -> anyhow::Result<Self> {
        let raw = crate::config::load_effective_config_disk_only()
            .context("failed to load Bladerunner config")?;
        Ok(Self::from_raw(&raw))
    }

    /// The normalized hostname, or an error explaining how to set it.
    pub fn require_hostname(&self) -> anyhow::Result<String> {
        let hostname = self.hostname.as_deref().map(str::trim).unwrap_or_default();
        if hostname.is_empty() {
            bail!(
                "No Foundry hostname is set. Use your Foundry address, e.g. \
                 `/login palantir-aip acme.palantirfoundry.com` or \
                 `bladerunner login --palantir-aip --hostname acme.palantirfoundry.com` \
                 (or export {HOSTNAME_ENV})."
            );
        }
        normalize_hostname(hostname)
    }

    pub fn require_client_id(&self) -> anyhow::Result<String> {
        let client_id = self.client_id.as_deref().map(str::trim).unwrap_or_default();
        if client_id.is_empty() {
            bail!(
                "No Bladerunner OAuth client ID is set for this Foundry. Register the app \
                 ({APP_SETUP_DOC}) and pass its client ID: `/login palantir-aip <hostname> \
                 <client-id>` or `bladerunner login --palantir-aip --client-id <id>`. Or sign in \
                 with a personal token: `/login palantir-aip token`."
            );
        }
        validate_client_id(client_id)?;
        Ok(client_id.to_owned())
    }
}

/// OAuth client IDs are printable ASCII (RFC 6749 VSCHAR); Foundry's can
/// contain dots (e.g. `ri.…`). Whitespace and control characters are refused
/// (the ID is URL-encoded wherever it's sent).
fn validate_client_id(client_id: &str) -> anyhow::Result<()> {
    if client_id.is_empty() || !client_id.chars().all(|c| c.is_ascii_graphic()) {
        bail!("`{client_id}` is not a valid Foundry OAuth client ID.");
    }
    Ok(())
}

/// Reduce `https://Host.example.com/` style input to a bare lowercase host
/// (with optional port), rejecting paths, credentials, and anything that is
/// not a plain hostname.
pub fn normalize_hostname(input: &str) -> anyhow::Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let url = url::Url::parse(&with_scheme)
        .map_err(|_| anyhow::anyhow!("`{input}` is not a valid Foundry hostname."))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Foundry hostname must be a bare https hostname such as `acme.palantirfoundry.com`.");
    }
    let Some(url::Host::Domain(host)) = url.host() else {
        bail!("Foundry hostname must be a domain name such as `acme.palantirfoundry.com`.");
    };
    let host = host.to_ascii_lowercase();
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Base URL for the hostname's Anthropic-compatible LLM proxy.
pub fn anthropic_proxy_base_url(hostname: &str) -> String {
    format!("https://{hostname}{ANTHROPIC_PROXY_PATH}")
}

/// Base URL used until a Foundry hostname is set. `.invalid` never resolves
/// (RFC 2606), so an unconfigured model fails closed instead of inheriting a
/// default endpoint that would receive the Foundry token.
const UNCONFIGURED_BASE_URL: &str = "https://foundry-hostname-not-set.invalid/";

/// The `palantir-aip` provider's base URL: the hostname's Anthropic proxy,
/// or an explicit `base_url` if it is on that same hostname.
pub(crate) fn provider_base_url(raw: &toml::Value, explicit: Option<&str>) -> String {
    let config = PalantirAipConfig::from_raw(raw);
    let Some(hostname) = config.hostname.as_deref() else {
        return UNCONFIGURED_BASE_URL.to_owned();
    };
    let hostname = match normalize_hostname(hostname) {
        Ok(hostname) => hostname,
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid [palantir_aip] hostname");
            return UNCONFIGURED_BASE_URL.to_owned();
        }
    };
    let derived = anthropic_proxy_base_url(&hostname);
    match explicit {
        Some(url) => pin_to_foundry(url, &derived),
        None => derived,
    }
}

/// `url` if it is https on the same host as `foundry_base_url`, else the
/// fail-closed URL. The token helper is bound to the configured hostname,
/// so an endpoint anywhere else (a stale override, a model-level pin) would
/// receive that Foundry's token.
pub(crate) fn pin_to_foundry(url: &str, foundry_base_url: &str) -> String {
    fn https_host(url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        if url.scheme() != "https" {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        Some(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        })
    }
    match (https_host(url), https_host(foundry_base_url)) {
        (Some(host), Some(foundry)) if host == foundry => url.to_owned(),
        _ => {
            tracing::warn!(
                url,
                "ignoring a palantir-aip base URL that isn't on the configured Foundry hostname"
            );
            UNCONFIGURED_BASE_URL.to_owned()
        }
    }
}

/// Pass the configured hostname to the bundled token helper
/// (`--hostname <host>`). The helper then mints for the same Foundry the
/// models' base URL points at, and since the auth provider's token cache is
/// keyed on its command line, a hostname change drops any cached token.
pub(crate) fn bind_auth_provider_hostname(
    providers: &mut indexmap::IndexMap<String, crate::auth::AuthProviderConfig>,
    raw: &toml::Value,
) {
    let Some(provider) = providers.get_mut(PROVIDER_ID) else {
        return;
    };
    let Some(args) = provider.args.as_mut() else {
        return;
    };
    if !args.iter().any(|arg| arg == "palantir-aip-token") {
        return;
    }
    // Any hostname already in the command line (a hand-written override) is
    // replaced: the helper must mint for the hostname the endpoint uses.
    let mut kept = Vec::with_capacity(args.len());
    let mut iter = std::mem::take(args).into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--hostname" {
            iter.next();
        } else if !arg.starts_with("--hostname=") {
            kept.push(arg);
        }
    }
    *args = kept;
    // Without a hostname the helper refuses on its own (no token is minted).
    if let Ok(hostname) = PalantirAipConfig::from_raw(raw).require_hostname() {
        args.extend(["--hostname".to_owned(), hostname]);
    }
}

/// The settings a sign-in will use: the saved config with any new hostname
/// or client ID applied, without saving anything. A new hostname drops the
/// saved client ID unless one is given (it belongs to the old Foundry).
pub fn staged_login_config(
    hostname: Option<&str>,
    client_id: Option<&str>,
) -> anyhow::Result<PalantirAipConfig> {
    let mut config = PalantirAipConfig::load()?;
    if let Some(hostname) = hostname {
        let hostname = normalize_hostname(hostname)?;
        if config.require_hostname().ok().as_deref() != Some(hostname.as_str()) {
            config.client_id = None;
        }
        config.hostname = Some(hostname);
    }
    if let Some(client_id) = client_id.map(str::trim) {
        validate_client_id(client_id)?;
        config.client_id = Some(client_id.to_owned());
    }
    Ok(config)
}

/// Write `[palantir_aip]` hostname/client_id into the user config.toml, keeping
/// everything else in the file as written. Returns what was there before, so
/// a change that can't take effect can be undone.
fn persist_user_config(hostname: &str, client_id: Option<&str>) -> anyhow::Result<ConfigBackup> {
    let path = xai_grok_config::grok_home().join("config.toml");
    // Write through a symlinked config.toml (even one whose target doesn't
    // exist yet) rather than replacing the link.
    let path = xai_grok_config::fs_atomic::resolve_write_target(&path)
        .context("failed to resolve config.toml")?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("failed to read config.toml"),
    };
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .context("config.toml has syntax errors; fix it before changing the Foundry hostname")?;
    let table = doc
        .entry("palantir_aip")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`palantir_aip` in config.toml is not a table"))?;
    table["hostname"] = toml_edit::value(hostname);
    if let Some(client_id) = client_id {
        table["client_id"] = toml_edit::value(client_id);
    }
    // config.toml can hold API keys: keep its mode, and create it owner-only.
    let mode = existing_mode(&path).unwrap_or(0o600);
    let existed = path.exists();
    write_atomic(&path, doc.to_string().as_bytes(), Some(mode))?;
    Ok(ConfigBackup {
        path,
        previous: existed.then_some(existing),
        mode,
    })
}

/// config.toml as it was before [`persist_user_config`].
struct ConfigBackup {
    path: PathBuf,
    previous: Option<String>,
    mode: u32,
}

impl ConfigBackup {
    fn restore(self) -> anyhow::Result<()> {
        match self.previous {
            Some(text) => write_atomic(&self.path, text.as_bytes(), Some(self.mode)),
            None => remove_if_present(&self.path).map(|_| ()),
        }
    }
}

/// The permission bits of an existing file (Unix only).
fn existing_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

// =============================================================================
// Credential storage
// =============================================================================

fn credentials_dir() -> PathBuf {
    xai_grok_config::grok_home().join("credentials")
}

pub fn oauth_credential_path() -> PathBuf {
    credentials_dir().join(OAUTH_CREDENTIAL_FILE)
}

pub fn token_path() -> PathBuf {
    credentials_dir().join(TOKEN_FILE)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct StoredCredential {
    hostname: String,
    client_id: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCredential")
            .field("hostname", &self.hostname)
            .field("client_id", &self.client_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Create the credentials directory owner-only and refuse a symlinked one.
fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        bail!("Palantir AIP credentials directory must not be a symlink.");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write via a same-directory temp file and rename. `mode` applies on Unix.
fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> anyhow::Result<()> {
    let dir = path.parent().expect("credential path has a parent");
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temporary = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(mode.unwrap_or(0o644));
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn read_credential(path: &Path) -> anyhow::Result<Option<StoredCredential>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .context("Palantir AIP credential file is corrupt; sign in again"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to read Palantir AIP credentials"),
    }
}

fn write_credential(path: &Path, credential: &StoredCredential) -> anyhow::Result<()> {
    ensure_private_dir(path.parent().expect("credential path has a parent"))?;
    let json = serde_json::to_vec_pretty(credential)?;
    write_atomic(path, &json, Some(0o600))
}

/// Exclusive advisory lock on `<credential>.lock`, released on drop.
struct CredentialLock(#[allow(dead_code)] std::fs::File);

/// How long to wait for another process's refresh or sign-in to finish.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// Take the credential lock, giving up after [`LOCK_WAIT`]. Polls with a
/// non-blocking try-lock so the wait really ends at the bound (a blocking
/// `flock` can't be interrupted, and would outlive a timeout around it).
fn lock_credential(path: &Path) -> anyhow::Result<CredentialLock> {
    use fs2::FileExt as _;
    let file = open_credential_lock(path)?;
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(CredentialLock(file)),
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => {
                return Err(error).context(
                    "timed out waiting for another Palantir AIP sign-in or token refresh",
                );
            }
        }
    }
}

/// [`lock_credential`] for async callers. The wait is a sleep between
/// try-locks, so dropping the future (e.g. on cancel) abandons it cleanly.
async fn lock_credential_async(path: &Path) -> anyhow::Result<CredentialLock> {
    use fs2::FileExt as _;
    let file = open_credential_lock(path)?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(CredentialLock(file)),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(error) => {
                return Err(error).context(
                    "timed out waiting for another Palantir AIP sign-in or token refresh",
                );
            }
        }
    }
}

fn open_credential_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    ensure_private_dir(path.parent().expect("credential path has a parent"))?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))
        .context("failed to open Palantir AIP credential lock")
}

/// A saved personal token and the Foundry it was entered for.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredToken {
    hostname: String,
    token: String,
}

/// Trim a pasted bearer, rejecting anything that isn't a single token.
fn clean_token(token: &str) -> Option<&str> {
    let token = token.trim();
    let token = token.strip_prefix("Bearer ").unwrap_or(token).trim();
    (!token.is_empty() && !token.chars().any(|c| c.is_whitespace() || c.is_control()))
        .then_some(token)
}

/// A pasted personal token, trimmed, or an error if it isn't one token.
pub fn validate_token(token: &str) -> anyhow::Result<String> {
    clean_token(token).map(str::to_owned).ok_or_else(|| {
        anyhow::anyhow!("Enter a single Foundry bearer token without spaces or line breaks.")
    })
}

fn remove_if_present(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Write a personal token for `hostname` and drop any OAuth sign-in, which
/// would otherwise take precedence. If the OAuth sign-in can't be removed,
/// the previous token is put back. The caller holds the credential lock.
fn write_token_credential(
    token_path: &Path,
    credential_path: &Path,
    hostname: &str,
    token: &str,
) -> anyhow::Result<()> {
    let stored = StoredToken {
        hostname: normalize_hostname(hostname)?,
        token: validate_token(token)?,
    };
    ensure_private_dir(token_path.parent().expect("token path has a parent"))?;
    let previous = match std::fs::read(token_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", token_path.display()));
        }
    };
    write_atomic(token_path, &serde_json::to_vec(&stored)?, Some(0o600))?;
    if let Err(error) = remove_if_present(credential_path) {
        let restored = match previous {
            Some(bytes) => write_atomic(token_path, &bytes, Some(0o600)),
            None => remove_if_present(token_path).map(drop),
        };
        if let Err(restore) = restored {
            tracing::warn!(%restore, "couldn't restore the previous Palantir AIP token");
        }
        return Err(error);
    }
    Ok(())
}

/// Remove both the OAuth credential and any personal token, under the
/// credential lock. Returns whether anything was removed.
pub fn clear_credentials() -> anyhow::Result<bool> {
    let _lock = lock_credential(&oauth_credential_path())?;
    let oauth = remove_if_present(&oauth_credential_path())?;
    let token = remove_if_present(&token_path())?;
    Ok(oauth || token)
}

// =============================================================================
// Token endpoint
// =============================================================================

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

enum TokenError {
    /// The grant was rejected (expired, revoked, or reused): sign in again.
    Rejected(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for TokenError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

impl TokenError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Rejected(detail) => {
                anyhow::anyhow!("Foundry sign-in is no longer valid ({detail}). {LOGIN_HINT}")
            }
            Self::Other(error) => error,
        }
    }
}

const NO_ERROR_CODE: &str = "no error code";

/// Whether a token-endpoint error means the grant or client is no good (sign
/// in again). Other errors, e.g. `temporarily_unavailable` or `server_error`,
/// may pass, so the stored credential is kept for the next attempt.
fn is_terminal_oauth_error(status: u16, code: &str) -> bool {
    matches!(
        code,
        "invalid_grant" | "invalid_client" | "unauthorized_client" | "invalid_scope"
    ) || (code == NO_ERROR_CODE && matches!(status, 401 | 403))
}

/// OAuth error code from a token-endpoint error body, without echoing the body.
fn oauth_error_code(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| NO_ERROR_CODE.to_owned())
}

async fn post_token(
    hostname: &str,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<(String, Option<String>, chrono::DateTime<chrono::Utc>), TokenError> {
    let request = crate::http::shared_oauth_client()
        .post(format!("https://{hostname}{TOKEN_PATH}"))
        .form(form);
    let response = tokio::time::timeout(HTTP_TIMEOUT, request.send())
        .await
        .map_err(|_| anyhow::anyhow!("Foundry token request timed out"))?
        .map_err(|e| anyhow::anyhow!("Foundry token request failed: {e}"))?;
    let status = response.status();
    let body = tokio::time::timeout(HTTP_TIMEOUT, response.text())
        .await
        .map_err(|_| anyhow::anyhow!("Foundry token response timed out"))?
        .map_err(|e| anyhow::anyhow!("Foundry token response unreadable: {e}"))?;
    if matches!(status.as_u16(), 400 | 401 | 403) {
        let code = oauth_error_code(&body);
        let detail = format!("HTTP {status}, {code}");
        return Err(if is_terminal_oauth_error(status.as_u16(), &code) {
            TokenError::Rejected(detail)
        } else {
            TokenError::Other(anyhow::anyhow!("Foundry token endpoint error ({detail})"))
        });
    }
    if !status.is_success() {
        return Err(anyhow::anyhow!("Foundry token endpoint returned HTTP {status}").into());
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|_| anyhow::anyhow!("Foundry token response was not valid JSON"))?;
    let Some(access) = parsed.access_token.filter(|t| !t.trim().is_empty()) else {
        return Err(anyhow::anyhow!("Foundry token response had no access_token").into());
    };
    if access.chars().any(char::is_control) {
        return Err(anyhow::anyhow!("Foundry access token contains control characters").into());
    }
    // Access-token lifetime is set per OAuth client; never assume one.
    let expires_in = parsed.expires_in.filter(|s| *s > 0).unwrap_or(600);
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(expires_in);
    let refresh = parsed
        .refresh_token
        .filter(|t| !t.trim().is_empty())
        .or_else(|| previous_refresh.map(str::to_owned));
    Ok((access, refresh, expires_at))
}

// =============================================================================
// Credential helper (`bladerunner auth palantir-aip-token`)
// =============================================================================

/// The token payload the auth provider parses.
#[derive(Debug, serde::Serialize)]
pub struct HelperToken {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
}

fn helper_token_from(credential: &StoredCredential) -> HelperToken {
    let remaining = (credential.expires_at - chrono::Utc::now()).num_seconds();
    HelperToken {
        access_token: credential.access_token.clone(),
        expires_in: Some((remaining.max(0) as u64).min(RECHECK_SECS)),
    }
}

fn is_fresh(credential: &StoredCredential) -> bool {
    let margin = if credential.refresh_token.is_some() {
        REFRESH_MARGIN_SECS
    } else {
        NO_REFRESH_MARGIN_SECS
    };
    credential.expires_at - chrono::Utc::now() > chrono::Duration::seconds(margin)
}

/// The helper found no usable credential: not signed in, signed in to
/// another Foundry, or the token was rejected. Unlike a failure that may pass
/// (network, lock timeout), this tells the agent to stop sending its cached
/// token: `bladerunner auth palantir-aip-token` exits with
/// [`crate::auth::AUTH_PROVIDER_SIGNED_OUT_EXIT_CODE`].
#[derive(Debug)]
pub struct NoCredential(String);

impl std::fmt::Display for NoCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoCredential {}

macro_rules! no_credential {
    ($($arg:tt)*) => {
        return Err(NoCredential(format!($($arg)*)).into())
    };
}

/// A usable bearer for Foundry model requests, in order: the OAuth access
/// token (refreshed under lock when near expiry), a pasted personal token,
/// then `FOUNDRY_TOKEN`. A sign-in made in Bladerunner wins over the shell.
///
/// `hostname` is the Foundry the caller will send the token to (the auth
/// provider passes it), overriding config so the two can't disagree. When the
/// auth provider reruns this after a 401 (`GROK_AUTH_PROVIDER_REJECTED=1`;
/// not `GROK_AUTH_EXPIRED`, which also marks routine cache expiry), the
/// rejected token (`GROK_AUTH_PROVIDER_ACCESS_TOKEN`) forces a refresh.
pub async fn helper_token(hostname: Option<&str>) -> anyhow::Result<HelperToken> {
    let mut config = PalantirAipConfig::load()?;
    if let Some(hostname) = hostname {
        config.hostname = Some(hostname.to_owned());
    }
    let env_token = env_token_for(
        &config,
        std::env::var(TOKEN_ENV).ok(),
        std::env::var(HOSTNAME_ENV).ok(),
    );
    let rejected = (std::env::var("GROK_AUTH_PROVIDER_REJECTED").as_deref() == Ok("1"))
        .then(|| std::env::var("GROK_AUTH_PROVIDER_ACCESS_TOKEN").ok())
        .flatten();
    helper_token_at(
        &config,
        &oauth_credential_path(),
        &token_path(),
        env_token,
        rejected.as_deref(),
    )
    .await
}

/// `FOUNDRY_TOKEN`, unless `FOUNDRY_HOSTNAME` says it belongs to a different
/// Foundry than the one in use (config wins over the environment hostname,
/// but never sends the environment's token somewhere else).
fn env_token_for(
    config: &PalantirAipConfig,
    env_token: Option<String>,
    env_hostname: Option<String>,
) -> Option<String> {
    let token = env_token?;
    let Some(env_hostname) = env_hostname.filter(|h| !h.trim().is_empty()) else {
        return Some(token);
    };
    let in_use = config.require_hostname().ok();
    if normalize_hostname(&env_hostname).ok() == in_use {
        return Some(token);
    }
    tracing::warn!(
        "ignoring {TOKEN_ENV}: {HOSTNAME_ENV} names a different Foundry than the one in use"
    );
    None
}

/// Whether `credential` can be served as is: not near expiry, and not the
/// token the server just rejected.
fn is_servable(credential: &StoredCredential, rejected: Option<&str>) -> bool {
    is_fresh(credential) && rejected != Some(credential.access_token.as_str())
}

async fn helper_token_at(
    config: &PalantirAipConfig,
    credential_path: &Path,
    token_path: &Path,
    env_token: Option<String>,
    rejected: Option<&str>,
) -> anyhow::Result<HelperToken> {
    // No token leaves without a hostname: the model's base URL comes from it.
    let configured = config
        .require_hostname()
        .map_err(|error| NoCredential(error.to_string()))?;
    // Credentials on disk for another Foundry are skipped, not fatal, so a
    // matching FOUNDRY_TOKEN still works; they only explain a failure below.
    let mut elsewhere = Vec::new();
    if let Some(credential) = read_credential(credential_path)? {
        if credential.hostname != configured {
            elsewhere.push(credential.hostname);
        } else if is_servable(&credential, rejected) {
            return Ok(helper_token_from(&credential));
        } else if let Some(token) = refresh_locked(credential_path, &configured, rejected).await? {
            return Ok(token);
        }
        // Otherwise the OAuth sign-in is spent (no refresh token, or Foundry
        // rejected it): fall through to a saved personal token or
        // FOUNDRY_TOKEN instead of failing every turn.
    }
    let mut legacy = false;
    let saved = match std::fs::read_to_string(token_path) {
        Ok(contents) => match serde_json::from_str::<StoredToken>(&contents) {
            Ok(stored) if stored.hostname != configured => {
                elsewhere.push(stored.hostname);
                None
            }
            Ok(stored) => Some(stored.token),
            // Saved by an older Bladerunner with no record of its Foundry: it
            // may belong to a different one, so it is never sent anywhere.
            Err(_) => {
                legacy = true;
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("failed to read the Palantir AIP token file"),
    };
    // A personal token Foundry rejected (on a 401 retry) is remembered by
    // fingerprint and skipped until the user signs in again, so later turns
    // fall back too instead of resending it.
    let rejected_path = rejected_tokens_path(token_path);
    let mut known_rejected = read_rejected_tokens(&rejected_path);
    if let Some(rejected) = rejected {
        let is_personal = |t: &Option<String>| t.as_deref().and_then(clean_token) == Some(rejected);
        if (is_personal(&saved) || is_personal(&env_token))
            && !known_rejected.contains(&token_fingerprint(rejected))
        {
            // Read-modify-write under the credential lock, so a rejection
            // recorded by another process meanwhile isn't lost.
            let _lock = lock_credential_async(credential_path).await?;
            known_rejected = read_rejected_tokens(&rejected_path);
            push_rejected_token(&mut known_rejected, token_fingerprint(rejected));
            remember_rejected_tokens(&rejected_path, &known_rejected);
        }
    }
    let not_rejected = |token: &String| {
        clean_token(token).is_some_and(|t| !known_rejected.contains(&token_fingerprint(t)))
    };
    let any_rejected = saved
        .iter()
        .chain(env_token.iter())
        .any(|t| !not_rejected(t));
    let saved = saved.filter(not_rejected);
    let env_token = env_token.filter(not_rejected);
    let (token, source) = match (saved, env_token) {
        (None, None) if any_rejected => no_credential!(
            "Foundry rejected the Palantir AIP token (saved or {TOKEN_ENV}); enter a new one. \
             {LOGIN_HINT}"
        ),
        (Some(token), _) => (token, "Palantir AIP token file"),
        (None, Some(token)) => (token, TOKEN_ENV),
        (None, None) if !elsewhere.is_empty() => no_credential!(
            "Palantir AIP credentials on this machine are for {} but config uses {configured}. \
             {LOGIN_HINT}",
            elsewhere.join(" and ")
        ),
        (None, None) if legacy => no_credential!(
            "The saved Palantir AIP token predates hostname checks; enter it again. {LOGIN_HINT}"
        ),
        (None, None) => no_credential!("Not signed in to Palantir AIP. {LOGIN_HINT}"),
    };
    let Some(token) = clean_token(&token) else {
        no_credential!("{source} is empty or malformed. {LOGIN_HINT}");
    };
    Ok(HelperToken {
        access_token: token.to_owned(),
        expires_in: Some(RECHECK_SECS),
    })
}

const REJECTED_TOKENS_FILE: &str = "foundry-aip.rejected";
/// Fingerprints kept; old ones fall off.
const REJECTED_TOKENS_MAX: usize = 32;

fn rejected_tokens_path(token_path: &Path) -> PathBuf {
    token_path.with_file_name(REJECTED_TOKENS_FILE)
}

/// SHA-256 of a token, so rejected tokens are recognized without storing them.
fn token_fingerprint(token: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn read_rejected_tokens(path: &Path) -> Vec<String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Add `fingerprint` as the newest entry, dropping the oldest past the cap.
fn push_rejected_token(fingerprints: &mut Vec<String>, fingerprint: String) {
    fingerprints.retain(|known| *known != fingerprint);
    fingerprints.push(fingerprint);
    let excess = fingerprints.len().saturating_sub(REJECTED_TOKENS_MAX);
    fingerprints.drain(..excess);
}

/// Best effort: failing to record a rejection only costs another 401 later.
fn remember_rejected_tokens(path: &Path, fingerprints: &[String]) {
    let result = serde_json::to_vec(fingerprints)
        .map_err(anyhow::Error::from)
        .and_then(|json| {
            ensure_private_dir(path.parent().expect("rejected list has a parent"))?;
            write_atomic(path, &json, Some(0o600))
        });
    if let Err(error) = result {
        tracing::warn!(%error, "couldn't record a rejected Palantir AIP token");
    }
}

/// Refresh under the credential lock. A sibling process may have refreshed
/// while this one waited, so the file is re-read after locking and the old
/// refresh token is never sent twice. `None` means the OAuth sign-in is
/// unusable for `hostname` (removed, replaced by another Foundry's, no
/// refresh token, or refresh rejected) and the caller should fall back.
async fn refresh_locked(
    credential_path: &Path,
    hostname: &str,
    rejected: Option<&str>,
) -> anyhow::Result<Option<HelperToken>> {
    let _lock = lock_credential_async(credential_path).await?;

    let Some(credential) = read_credential(credential_path)? else {
        return Ok(None);
    };
    // A sign-in to another Foundry may have replaced the file meanwhile.
    if credential.hostname != hostname {
        return Ok(None);
    }
    // Another process may have refreshed while this one waited: serve its
    // token unless it is the one the server rejected.
    if is_servable(&credential, rejected) {
        return Ok(Some(helper_token_from(&credential)));
    }
    let Some(refresh) = credential.refresh_token.clone() else {
        // Can't refresh. If Foundry just rejected this access token, the
        // sign-in is spent: remove it so later turns don't select it again.
        if rejected == Some(credential.access_token.as_str()) {
            remove_if_present(credential_path)?;
        }
        return Ok(None);
    };
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", credential.client_id.as_str()),
    ];
    let (access, refresh, expires_at) =
        match post_token(&credential.hostname, &form, Some(&refresh)).await {
            Ok(tokens) => tokens,
            // Foundry refused the refresh token (revoked or expired): the
            // sign-in is spent, so drop it rather than retry it every turn.
            Err(TokenError::Rejected(detail)) => {
                tracing::warn!(%detail, "Foundry rejected the OAuth refresh; signing out");
                remove_if_present(credential_path)?;
                return Ok(None);
            }
            Err(TokenError::Other(error)) => return Err(error),
        };
    let updated = StoredCredential {
        access_token: access,
        refresh_token: refresh,
        expires_at,
        ..credential
    };
    write_credential(credential_path, &updated)?;
    Ok(Some(helper_token_from(&updated)))
}

// =============================================================================
// Interactive sign-in
// =============================================================================

/// Where Foundry sends the browser after consent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PalantirAipLoginMethod {
    /// Loopback listener on 127.0.0.1 (any port); pasting the redirected
    /// URL also works when the listener can't be reached.
    Browser,
    /// Foundry's hosted callback page shows a code to paste. For SSH sessions,
    /// where the browser runs on another machine.
    Paste,
}

impl PalantirAipLoginMethod {
    /// `Paste` inside an SSH session, `Browser` otherwise.
    pub fn default_for_environment() -> Self {
        if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
            Self::Paste
        } else {
            Self::Browser
        }
    }
}

fn build_authorize_url(
    hostname: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut url = url::Url::parse(&format!("https://{hostname}{AUTHORIZE_PATH}"))
        .expect("authorize URL parses");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

type CallbackResult = Result<(String, Option<String>), String>;

async fn handle_callback(
    State(tx): State<tokio::sync::mpsc::Sender<CallbackResult>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let result = if let Some(error) = params.get("error") {
        Err(match params.get("error_description") {
            Some(desc) if !desc.is_empty() => format!("{error}: {desc}"),
            _ => error.clone(),
        })
    } else {
        match params.get("code") {
            Some(code) => Ok((code.clone(), params.get("state").cloned())),
            None => Err("Missing authorization code.".to_owned()),
        }
    };
    let ok = result.is_ok();
    let _ = tx.try_send(result);
    let (title, message) = if ok {
        (
            "Signed in",
            "Palantir AIP sign-in completed. You can close this window and return to Bladerunner.",
        )
    } else {
        ("Sign-in failed", "Close this window and try again.")
    };
    (StatusCode::OK, Html(callback_page(title, message, ok)))
}

/// Race the loopback callback (when listening) against a pasted code or URL.
async fn wait_for_code(
    expected_state: &str,
    method: PalantirAipLoginMethod,
    code_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    listener: Option<TcpListener>,
) -> anyhow::Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackResult>(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let server = listener.map(|listener| {
        let app = Router::new()
            .route("/", get(handle_callback))
            .with_state(tx.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        })
    });

    match code_rx {
        Some(mut code_rx) => {
            let paste_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(input) = code_rx.recv().await {
                    if let Some(callback) = parse_authorization_input(&input) {
                        let _ = paste_tx.send(Ok((callback.code, callback.state))).await;
                        return;
                    }
                }
            });
        }
        None if std::io::stdin().is_terminal() => {
            let paste_tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::BufRead as _;
                let mut line = String::new();
                loop {
                    if paste_tx.is_closed() {
                        return;
                    }
                    line.clear();
                    match std::io::stdin().lock().read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    if let Some(callback) = parse_authorization_input(&line) {
                        let _ = paste_tx.blocking_send(Ok((callback.code, callback.state)));
                        return;
                    }
                }
            });
        }
        None => {}
    }
    drop(tx);

    let result = tokio::time::timeout(LOGIN_TIMEOUT, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("Palantir AIP sign-in timed out after 15 minutes"))?
        .ok_or_else(|| anyhow::anyhow!("Palantir AIP sign-in cancelled"))?;
    let _ = shutdown_tx.send(());
    if let Some(server) = server {
        let _ = server.await;
    }
    let (code, state) = result.map_err(|e| anyhow::anyhow!("Foundry sign-in failed: {e}"))?;
    check_state(method, state.as_deref(), expected_state)?;
    Ok(code)
}

/// CSRF check. A browser redirect always carries the `state` we issued, so a
/// missing one is rejected; only the hosted paste page shows a bare code.
fn check_state(
    method: PalantirAipLoginMethod,
    state: Option<&str>,
    expected_state: &str,
) -> anyhow::Result<()> {
    match (state, method) {
        (Some(state), _) if state == expected_state => Ok(()),
        (None, PalantirAipLoginMethod::Paste) => Ok(()),
        (None, PalantirAipLoginMethod::Browser) => {
            bail!("Foundry sign-in returned no state; paste the full redirect URL and try again.")
        }
        (Some(_), _) => bail!("Foundry sign-in state mismatch; try again."),
    }
}

/// Tokens from a completed OAuth sign-in, not yet saved.
pub struct OAuthSignIn(StoredCredential);

/// The cancellable half of an OAuth sign-in: browser consent and
/// the code exchange. Nothing is saved, so dropping it leaves no trace.
///
/// `config` is the hostname/client ID to sign in with; a caller changing
/// them stages them here and saves them only once sign-in succeeds.
pub async fn obtain_oauth_credential(
    channels: Option<AuthChannels>,
    method: PalantirAipLoginMethod,
    config: &PalantirAipConfig,
) -> anyhow::Result<OAuthSignIn> {
    let hostname = config.require_hostname()?;
    let client_id = config.require_client_id()?;

    let (listener, redirect_uri) = match method {
        PalantirAipLoginMethod::Browser => {
            // Foundry matches a loopback redirect without its port, so any
            // free port works (as in Palantir's own OAuth client).
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("could not listen on 127.0.0.1 for the sign-in callback")?;
            let port = listener.local_addr()?.port();
            (Some(listener), format!("http://127.0.0.1:{port}/"))
        }
        PalantirAipLoginMethod::Paste => {
            (None, format!("https://{hostname}{HOSTED_CALLBACK_PATH}"))
        }
    };

    let pkce = generate_pkce();
    let state = create_state();
    let auth_url = build_authorize_url(
        &hostname,
        &client_id,
        &redirect_uri,
        &pkce.challenge,
        &state,
    );

    let (url_tx, code_rx) = match channels {
        Some(channels) => (channels.url_tx, Some(channels.code_rx)),
        None => (None, None),
    };
    if let Some(tx) = url_tx {
        let _ = tx.send(AuthUrlInfo {
            url: auth_url.clone(),
            mode: AuthUrlMode::Loopback,
        });
    } else {
        eprintln!();
        eprintln!("To sign in to Palantir AIP ({hostname}), open this URL in your browser:");
        eprintln!();
        eprintln!("  {auth_url}");
        eprintln!();
        if std::io::stdin().is_terminal() {
            match method {
                PalantirAipLoginMethod::Browser => {
                    eprintln!("Finish in the browser, or paste the URL it redirects to here:")
                }
                PalantirAipLoginMethod::Paste => {
                    eprintln!("After approving, paste the code Foundry shows here:")
                }
            }
        }
    }
    if method == PalantirAipLoginMethod::Browser {
        let url = auth_url.clone();
        if let Ok(Err(error)) = tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
            tracing::info!(%error, "palantir-aip auth: could not open browser");
        }
    }

    let code = wait_for_code(&state, method, code_rx, listener).await?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", pkce.verifier.as_str()),
    ];
    let (access, refresh, expires_at) =
        post_token(&hostname, &form, None)
            .await
            .map_err(|e| match e {
                TokenError::Rejected(detail) => {
                    anyhow::anyhow!("Foundry rejected the sign-in ({detail}); try again.")
                }
                TokenError::Other(error) => error,
            })?;
    if refresh.is_none() {
        tracing::warn!("palantir-aip auth: Foundry returned no refresh token");
    }
    Ok(OAuthSignIn(StoredCredential {
        hostname,
        client_id,
        access_token: access,
        refresh_token: refresh,
        expires_at,
    }))
}

/// A credential from a completed sign-in, not yet saved.
pub enum SignedIn {
    OAuth(OAuthSignIn),
    Token(PastedToken),
}

impl SignedIn {
    fn hostname(&self) -> &str {
        match self {
            Self::OAuth(OAuthSignIn(credential)) => &credential.hostname,
            Self::Token(pasted) => &pasted.hostname,
        }
    }
}

/// Save a completed sign-in: its Foundry settings and its credential, as one
/// step under the config write lock and the credential lock. Callers await
/// this outside any cancellation, so a sign-in reported as cancelled saved
/// nothing.
///
/// `new_hostname` is set when the user asked for this Foundry; otherwise the
/// sign-in is for whichever Foundry config named when it started. Nothing is
/// written if `still_current` says a newer sign-in replaced this one, or if
/// config (in any process) has since moved to another Foundry. A hostname
/// that managed configuration overrides is rolled back out of the user
/// config, and the saved token for the Foundry still in use is kept.
pub async fn commit_sign_in(
    new_hostname: Option<&str>,
    client_id: Option<&str>,
    signed_in: SignedIn,
    still_current: impl Fn() -> bool,
) -> anyhow::Result<()> {
    let hostname = signed_in.hostname().to_owned();
    let client_id = client_id.map(str::trim);
    if let Some(client_id) = client_id {
        validate_client_id(client_id)?;
    }
    let config_lock = crate::util::config::lock_config_writes().await?;
    let credential_lock = lock_credential_async(&oauth_credential_path()).await?;
    if !still_current() {
        bail!(SUPERSEDED);
    }
    let current = PalantirAipConfig::load()?.require_hostname().ok();
    let changes_hostname = current.as_deref() != Some(hostname.as_str());
    if changes_hostname && new_hostname.is_none() {
        // Signed in to the Foundry config named at the start, but config has
        // since moved elsewhere (another process): don't move it back.
        bail!(SUPERSEDED);
    }
    // An explicitly requested hostname is saved even when FOUNDRY_HOSTNAME
    // already names it, so the sign-in survives that variable going away.
    let backup = if changes_hostname || new_hostname.is_some() || client_id.is_some() {
        // A client ID belongs to one Foundry: a hostname change without a
        // new one clears the old.
        let backup = persist_user_config(&hostname, client_id.or(changes_hostname.then_some("")))?;
        // A managed or requirements layer can pin these above the user
        // config; then the change has no effect and is undone.
        let effective = PalantirAipConfig::load()?;
        let effective_hostname = effective.require_hostname().ok();
        if effective_hostname.as_deref() != Some(hostname.as_str()) {
            backup.restore()?;
            bail!(
                "The Foundry hostname is set to {} by managed configuration and can't be \
                 changed to {hostname} here.",
                effective_hostname.as_deref().unwrap_or("another value")
            );
        }
        if let Some(client_id) = client_id
            && effective.client_id.as_deref().map(str::trim) != Some(client_id)
        {
            backup.restore()?;
            bail!(
                "The Foundry OAuth client ID is set by managed configuration and can't be \
                 changed here."
            );
        }
        Some(backup)
    } else {
        None
    };
    // An OAuth credential must belong to the client ID config now selects:
    // another process may have switched it (same Foundry) meanwhile.
    if let SignedIn::OAuth(OAuthSignIn(credential)) = &signed_in {
        let effective = PalantirAipConfig::load()?.client_id;
        if effective.as_deref().map(str::trim) != Some(credential.client_id.as_str()) {
            if let Some(backup) = backup {
                backup.restore()?;
            }
            bail!(SUPERSEDED);
        }
    }
    // Save the new credential before removing anything of the old one; if
    // that fails, the settings change is undone and the old login still works.
    let is_oauth = matches!(signed_in, SignedIn::OAuth(_));
    let saved = match signed_in {
        SignedIn::OAuth(OAuthSignIn(credential)) => {
            write_credential(&oauth_credential_path(), &credential)
        }
        SignedIn::Token(pasted) => write_token_credential(
            &token_path(),
            &oauth_credential_path(),
            &pasted.hostname,
            &pasted.token,
        ),
    };
    if let Err(error) = saved {
        if let Some(backup) = backup {
            backup.restore()?;
        }
        return Err(error);
    }
    // A fresh sign-in starts with a clean slate (the user may re-enter a
    // token Foundry once rejected, now that it works).
    if let Err(error) = remove_if_present(&rejected_tokens_path(&token_path())) {
        tracing::warn!(%error, "couldn't clear the rejected Palantir AIP tokens");
    }
    if changes_hostname && is_oauth {
        // The saved personal token belongs to the old Foundry. (A new token
        // sign-in has already replaced it.) The sign-in is already saved, so
        // a failure here is only logged: the leftover token records its
        // hostname and is never sent to the new Foundry.
        if let Err(error) = remove_if_present(&token_path()) {
            tracing::warn!(%error, "couldn't remove the previous Foundry's saved token");
        }
    }
    // Release before touching the provider cache: a turn minting a token
    // holds that cache while its helper waits on the credential lock.
    drop(credential_lock);
    drop(config_lock);
    crate::auth::auth_provider::invalidate_provider_token(PROVIDER_ID).await;
    Ok(())
}

/// What a client asks `/login palantir-aip` to do, carried in the ACP
/// authenticate request's `_meta` under [`LoginRequest::META_KEY`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LoginRequest {
    /// Paste a personal token instead of browser OAuth.
    pub token: bool,
    /// Foundry hostname to save before signing in.
    pub hostname: Option<String>,
    /// OAuth client ID to save before signing in.
    pub client_id: Option<String>,
}

/// Key under [`LoginRequest::META_KEY`] in a successful authenticate
/// response: a model this client should start on (see the agent's
/// `suggested_palantir_model`).
pub const SUGGESTED_MODEL_KEY: &str = "suggestedModel";

impl LoginRequest {
    pub const META_KEY: &str = "palantirAip";

    pub fn from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> Self {
        meta.and_then(|m| m.get(Self::META_KEY))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    pub fn to_meta_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// Foundry's page for creating a personal token.
pub fn token_settings_url(hostname: &str) -> String {
    format!("https://{hostname}{TOKEN_SETTINGS_PATH}")
}

/// Wait for a personal token pasted into the client UI, after pushing
/// Foundry's token page as the URL to open. Saving is a separate step
/// ([`commit_sign_in`]) so a caller can let the user cancel the wait but not
/// a save in progress. Main caller: `/login palantir-aip token` over ACP.
pub async fn receive_pasted_token(
    channels: AuthChannels,
    config: &PalantirAipConfig,
) -> anyhow::Result<PastedToken> {
    let hostname = config.require_hostname()?;
    let AuthChannels {
        url_tx,
        mut code_rx,
    } = channels;
    if let Some(tx) = url_tx {
        let _ = tx.send(AuthUrlInfo {
            url: token_settings_url(&hostname),
            mode: AuthUrlMode::Loopback,
        });
    }
    let token = tokio::time::timeout(LOGIN_TIMEOUT, code_rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("Palantir AIP sign-in timed out after 15 minutes"))?
        .ok_or_else(|| anyhow::anyhow!("Palantir AIP sign-in cancelled"))?;
    // Checked here, before the caller saves any settings for this sign-in.
    let token = validate_token(&token)?;
    Ok(PastedToken { hostname, token })
}

/// A token pasted at sign-in, with the Foundry whose token page was shown.
#[derive(Debug)]
pub struct PastedToken {
    hostname: String,
    token: String,
}

impl PastedToken {
    /// A token entered for `hostname` (e.g. at a terminal prompt), validated.
    pub fn new(hostname: &str, token: &str) -> anyhow::Result<Self> {
        Ok(Self {
            hostname: normalize_hostname(hostname)?,
            token: validate_token(token)?,
        })
    }
}

#[cfg(test)]
#[path = "palantir_aip_tests.rs"]
mod tests;
