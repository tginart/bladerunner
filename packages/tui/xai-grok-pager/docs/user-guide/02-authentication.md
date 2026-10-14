# Authentication

Bladerunner keeps provider credentials independent. Authenticating with Codex or xAI/Grok enables that adapter; it does not bind the harness itself to the provider.

| Method | How | Typical use |
|--------|-----|-------------|
| xAI/Grok OAuth | `bladerunner login` or `g` on welcome | xAI-hosted models |
| xAI API key | `export XAI_API_KEY=…` | CI / no browser |
| OpenAI Codex (ChatGPT) | `bladerunner login --openai` / `/login codex` | ChatGPT Plus/Pro coding |
| Palantir AIP (Foundry) | `bladerunner login --palantir-aip` / `/login palantir-aip` | AIP language models on your Foundry |
| Custom model credentials | `[model.<name>]` or provider-standard environment keys | Compatible user-defined endpoints |
| Customer OIDC / external auth | config / env | Managed or self-hosted deployments |

Credentials live under `~/.bladerunner/auth.json` in separate scopes. Logging out of one provider does not erase another provider's session.

---

## First launch (Bladerunner community builds)

Bladerunner does not force either provider's browser login. You land on the welcome splash:

1. Press **`g`** to sign in with xAI / Grok, or **`c`** to sign in with OpenAI Codex.
2. Or enter a session after any credential is already configured, then use explicit slash commands:

```text
/login grok            # xAI / Grok OAuth
/login codex           # ChatGPT Codex OAuth
/login palantir-aip    # Palantir AIP (Foundry): OAuth, or add `token`
```

If one provider is unavailable, switch to another configured provider or use an explicit compatible endpoint. Provider account state never locks the entire harness.

---

## Palantir AIP (Foundry)

Bladerunner ships no Foundry address. The first sign-in asks for your Foundry
hostname (the address in the browser when Foundry is open) and saves it as
`[palantir_aip] hostname`. Choose one of two sign-in methods:

```text
/login palantir-aip <hostname> <client-id>   # browser OAuth
/login palantir-aip token <hostname>         # personal token
```

From a terminal, `bladerunner login --palantir-aip` asks for anything missing;
`--hostname`, `--client-id`, `--token`, and `--no-browser` set them directly.

**Browser OAuth** (authorization code with PKCE) needs the Bladerunner app
registered once on your Foundry: an admin follows `docs/foundry-app.md` in the
Bladerunner repository and shares the client ID. Bladerunner is a public
client, so no secret is shipped, and it requests only
`api:use-language-models-execute` (call any AIP model your account can use) and
`offline_access` (a refresh token). Inside SSH, or with `--no-browser`, Foundry
shows a code to paste instead of redirecting to `127.0.0.1`. The credential is
stored in `~/.bladerunner/credentials/foundry-aip-oauth.json` (owner-only).
Foundry rotates the refresh token on every use; Bladerunner refreshes under a
lock so parallel sessions never reuse one. A refresh token unused for 30 days
expires, and you sign in again.

**Personal token** needs no Foundry setup. The app opens Foundry's token page
and takes the token in a masked field. Bladerunner also reads Palantir's SDK
variables, `FOUNDRY_HOSTNAME` and `FOUNDRY_TOKEN`. When several credentials
exist, the OAuth sign-in wins, then a token saved at sign-in, then
`FOUNDRY_TOKEN`.

`bladerunner logout --palantir-aip` removes the saved credentials.

---

## xAI/Grok browser login

Press `g` on the welcome screen, run `/login grok`, or run the CLI login command to authenticate the xAI/Grok adapter:

```bash
bladerunner
# or
bladerunner login
```

Credentials are stored in `~/.bladerunner/auth.json` and reused across sessions. Access tokens refresh in the background. When a token can't be refreshed, you are prompted to sign in again. Credentials without a server-provided expiry fall back to a 30-day lifetime.

### Credential storage

Tokens in `~/.bladerunner/auth.json` (and MCP OAuth tokens in `~/.bladerunner/mcp_credentials.json`) are written with owner-only permissions (`0600` on Unix). Anyone with filesystem access to those paths can use the credentials, so:

- Prefer full-disk encryption (FileVault, BitLocker, LUKS, or equivalent).
- Do not copy `auth.json` or `mcp_credentials.json` into shared directories, tickets, or chat.
- On multi-user hosts, keep `$HOME` / `$BLADERUNNER_HOME` private to your account.

### Re-authenticate

To switch accounts or resolve an authentication problem, run:

```bash
bladerunner login
```

Running `bladerunner login` starts the xAI/Grok sign-in flow again. Pass `--openai` to select Codex instead:

| Flag | Description |
|------|-------------|
| `--oauth` | Sign in through xAI OAuth. This is the default provider for the bare login command. |
| `--device-auth` (alias `--device-code`) | Sign in with the xAI device-code flow for headless or remote environments. |
| `--openai` | Sign in with a ChatGPT Plus/Pro subscription for OpenAI Codex. |

To sign out of the xAI/Grok session, run `bladerunner logout`. Codex remains until explicitly cleared.

Third-party subscriptions and BYOK keys are separate:

| Command | Clears |
|---------|--------|
| `bladerunner logout` | xAI/Grok session |
| `bladerunner logout --openai` | OpenAI Codex (ChatGPT) OAuth only |
| `bladerunner logout --all` | All stored OAuth sessions, but not environment variables |

`XAI_API_KEY` and other env-based keys are never removed by logout — unset the
environment variable yourself.

---

## API Key

For CI/CD, automation, or environments without browser access, use an API key from [console.x.ai](https://console.x.ai):

```bash
export XAI_API_KEY="xai-..."
bladerunner
```

Bladerunner uses the API key as a fallback when no session token is active. If you have already signed in interactively, the stored session token takes precedence. To fall back to the API key, run `bladerunner logout` (xAI session only) or delete the relevant scopes in `~/.bladerunner/auth.json`.

---

## OIDC (Customer SSO)

Authenticate developers through your own Identity Provider (IdP)—such as Okta, Azure AD, or Auth0—instead of a provider's consumer login.

### 1. Register a public client in your IdP

- Grant type: Authorization Code with PKCE (Proof Key for Code Exchange)
- Redirect URI: `http://127.0.0.1/callback` -- a loopback address. Bladerunner binds a random port at sign-in time, and most IdPs treat the loopback redirect as port-agnostic per [RFC 8252](https://tools.ietf.org/html/rfc8252).
- No client secret. PKCE replaces it.

### 2. Configure the CLI

Via config file:

```toml
# ~/.bladerunner/config.toml
[bladerunner_com_config.oidc]
issuer = "https://acme.okta.com"
client_id = "0oa1b2c3d4e5f6g7h8i9"
```

Or via environment variables:

```bash
export BLADERUNNER_OIDC_ISSUER="https://acme.okta.com"
export BLADERUNNER_OIDC_CLIENT_ID="0oa1b2c3d4e5f6g7h8i9"
```

You can also override the API endpoint to point at your own proxy:

```bash
export BLADERUNNER_CLI_CHAT_PROXY_BASE_URL="https://models.acme.com/v1"
```

### 3. Run `bladerunner`

The CLI discovers endpoints via `{issuer}/.well-known/openid-configuration`, opens the IdP login page, and stores tokens in `~/.bladerunner/auth.json`. Tokens auto-refresh silently via the stored `refresh_token`.

### Optional fields

| Field | Default | Notes |
|-------|---------|-------|
| `scopes` | `["openid", "profile", "email", "offline_access", "api:access"]` | `offline_access` enables silent token refresh |
| `audience` | None | Required by some IdPs (e.g., Auth0) |

---

## External Auth Provider

When browser-based login isn't possible -- for example, on sandboxed VMs, CI runners, or air-gapped networks -- delegate authentication to an external binary or script.

### How It Works

```
+--------------+     sh -c     +------------------------+
|     Bladerunner     |-------------->|  your auth binary      |
|              |               |                        |
|  reads       |<-- stdout ----|  prints token          |
|  auth.json   |               |                        |
|              |   (stderr)    |  prints status/URLs    |--> surfaced to user
+--------------+               +------------------------+
```

1. Bladerunner runs your command via `sh -c "<command>"`
2. Your binary runs whatever auth flow it needs (SSO, device code, certificate exchange)
3. **stderr** carries human-readable output, such as login URLs and status messages. Bladerunner reads stderr and surfaces it to the user; in the TUI, it turns the first `https://` URL into a clickable sign-in link.
4. **stdout** is captured by Bladerunner and saved as the access token
5. Exit 0 = success; exit non-zero = Bladerunner falls back to interactive login

### The stdout / stderr Contract

| Stream | What to print | Who sees it |
|--------|---------------|-------------|
| **stdout** | The token -- nothing else | Bladerunner (parsed and stored in auth.json) |
| **stderr** | Login URLs, status messages, errors | The user (Bladerunner reads stderr and shows the sign-in URL as a clickable link in the TUI) |

**Do not print anything to stdout except the token.** No progress messages, no debug output. Bladerunner reads stdout, trims surrounding whitespace, and parses the result as a token.

### stdout Token Format

**Bare string** -- just the raw token:

```
eyJhbGciOiJSUzI1NiIs...
```

**JSON** -- with optional refresh token, expiry, and issuer:

```json
{"access_token": "eyJhbGciOi...", "refresh_token": "ref-tok", "expires_in": 3600, "issuer": "https://idp.example.com"}
```

Use JSON if your tokens expire and you want Bladerunner to automatically re-run the binary before expiry.

JSON fields:

| Field | Required | Meaning |
|-------|----------|---------|
| `access_token` | yes | Bearer token Bladerunner sends to the xAI API |
| `refresh_token` | no | Stored for reference. Bladerunner refreshes by re-running your binary, not with an OAuth refresh grant |
| `expires_in` | no | Token lifetime in seconds; enables proactive refresh before expiry |
| `issuer` | no | Identifies the token's issuer |

### Configuration

Via config file:

```toml
# ~/.bladerunner/config.toml
[auth]
auth_provider_command = "/usr/local/bin/my-auth-provider"
auth_provider_label = "Acme Corp"   # optional -- customizes the TUI login button
auth_token_ttl = 3600               # optional -- token lifetime in seconds
```

Or via environment variables:

```bash
export GROK_AUTH_PROVIDER_COMMAND="/usr/local/bin/my-auth-provider"
export GROK_AUTH_PROVIDER_LABEL="Acme Corp"
export GROK_AUTH_TOKEN_TTL=3600
```

### Token Refresh

Bladerunner runs your binary on two different contracts, and `GROK_AUTH_EXPIRED` is how
it tells them apart. Each run fully replaces the stored credential, so emit the
same JSON fields (such as `issuer`) on every invocation, including refreshes.

- **`GROK_AUTH_EXPIRED=1` — a headless refresh.** Bladerunner is re-minting over a
  credential it already holds: a near-expiry rotation, or a token the server
  rejected. Nobody is watching. stdin is closed, your stderr is swallowed, and
  the binary is given a few seconds before it is killed. Mint silently or exit
  non-zero — never block.
- **Unset — a sign-in.** `bladerunner login`, the sign-in screen, or the escalation
  Bladerunner performs when a headless run couldn't mint. A user is waiting, your
  stderr reaches them, and you have 300 seconds — enough for a browser round
  trip or a device code.

```bash
#!/bin/sh
if [ "$GROK_AUTH_EXPIRED" = "1" ]; then
    # Headless: silent refresh only. Declining is the fast, correct answer
    # when your SSO session has lapsed and only the user can renew it.
    echo "Refreshing token..." >&2
    TOKEN=$(my-company-auth --refresh --silent) || exit 1
else
    echo "Authenticating via Acme Corp SSO..." >&2
    TOKEN=$(my-company-auth --login --interactive)
fi

if [ -z "$TOKEN" ]; then
    echo "Authentication failed" >&2
    exit 1
fi

echo "{\"access_token\": \"$TOKEN\", \"expires_in\": 3600}"
```

When the headless run can't produce a token, Bladerunner stops treating the stored
credential as usable and starts the sign-in flow instead — the same one you get
on a machine that has never signed in, with your binary's stderr shown, so a
device-code URL or a browser prompt reaches you. Exiting promptly on
`GROK_AUTH_EXPIRED=1` is what makes that handover fast; a binary that blocks
instead makes you wait out the refresh timeout on every start. Mid-session, the
turn fails with a re-auth prompt and `/login` re-runs the binary interactively.

One case stays ambiguous, and only in **leader mode** (`--leader`, or
`[cli] use_leader = true`; off by default): with no credential at all, the
leader makes one extra attempt in the background just after startup, and that
run has the variable unset, like a sign-in. A binary that mints without help
(service account, keytab, mounted token) succeeds there and the session heals
itself. One that must prompt just sits, up to the 300s sign-in ceiling —
nothing waits on it, the sign-in screen is already up, and that run's stderr
goes to `~/.bladerunner/leader.log` rather than to you.

### Environment Variables

| Variable | Description |
|----------|-------------|
| `GROK_AUTH_PROVIDER_COMMAND` | Path to your auth binary |
| `GROK_AUTH_PROVIDER_LABEL` | Display name on the TUI login screen (e.g., "Acme Corp") |
| `GROK_AUTH_TOKEN_TTL` | Token lifetime in seconds (for bare-string tokens without `expires_in`) |
| `GROK_AUTH_EXPIRED` | Set to `1` on a headless refresh: don't prompt, and don't hand back a cached token. Unset on a sign-in, where a user is attached |
| `GROK_AUTH_EARLY_INVALIDATION_SECS` | Seconds before expiry to proactively refresh (default: 300) |

---

## Device Code Flow

For headless environments (SSH sessions, Docker containers, remote VMs) where no browser is available locally:

```bash
bladerunner login --device-auth    # or: bladerunner login --device-code
```

This prints a URL and code to the terminal. Open the URL on any device, enter the code, and complete authentication. Bladerunner polls until the login is confirmed.

You can also implement the device-code flow through an [External Auth Provider](#external-auth-provider) for full control.

---

## Automatic Credential Refresh

Bladerunner automatically refreshes expired credentials:

- **Before expiry:** If your auth provider returned `expires_in` (JSON output) or you set `auth_token_ttl`, Bladerunner re-runs the auth binary ~5 minutes before expiry.
- **On auth error:** If the server returns 401 Unauthorized, Bladerunner refreshes the credentials and retries the request.
- **OIDC:** If a `refresh_token` is available, Bladerunner silently refreshes via your IdP without re-opening the browser.

Tune the refresh buffer:

```bash
# Refresh 5 minutes before expiry (default)
export GROK_AUTH_EARLY_INVALIDATION_SECS=300

# Disable the proactive buffer: refresh at expiry or on a 401 (set to 0)
export GROK_AUTH_EARLY_INVALIDATION_SECS=0
```

---

## Hot Reload

Bladerunner picks up changes to `~/.bladerunner/auth.json` automatically. If you update credentials externally (for example, with a script that writes new tokens), Bladerunner uses the new credentials on the next API call without a restart.

---

## Auth Precedence

Bladerunner resolves credentials for each request in this order, highest to lowest:

1. **Per-model `api_key` or `env_key`** -- set under `[model.<name>]` in `config.toml`. Wins whenever present.
2. **Active session token** -- obtained through browser, OIDC/OAuth2, or external-provider login and stored in `~/.bladerunner/auth.json`.
3. **`XAI_API_KEY`** -- fallback when no session token is active.

When more than one login flow is configured, Bladerunner populates the session token from the first available source, highest to lowest:

1. **External auth provider** (`auth_provider_command`)
2. **Enterprise OIDC** -- when OIDC is configured, through `[bladerunner_com_config.oidc]` in `config.toml` or the `GROK_OIDC_ISSUER` and `GROK_OIDC_CLIENT_ID` environment variables
3. **xAI/Grok OAuth2 browser login** -- the default for the bare xAI login command

During a session, the active method handles all mid-session refreshes.

---

## Related settings

Bladerunner always opts out of coding-data retention and training. The
upstream settings row and `/privacy` command are not exposed. This policy does
not change these independent config knobs:

| Setting | How to set it |
|---------|---------------|
| `[features] telemetry` | `config.toml` or `GROK_TELEMETRY_ENABLED` |
| `[telemetry] trace_upload` | `config.toml` or `GROK_TELEMETRY_TRACE_UPLOAD` |
| External OpenTelemetry | `GROK_EXTERNAL_OTEL` / `[telemetry] otel_*`. See [Monitoring Usage](24-monitoring-usage.md). |

See [Monitoring Usage](24-monitoring-usage.md#related-settings) and [Configuration](05-configuration.md#telemetry).

---

## Troubleshooting

### Debug logging

Set `RUST_LOG` to control the verbosity of the file log and headless stderr output. (The TUI's on-screen tracing pane uses a fixed filter and ignores `RUST_LOG`.) In the TUI, file logging defaults to `DEBUG`; in headless mode (`-p`), `RUST_LOG` defaults to `off` so only the answer is printed — set `RUST_LOG=error` (or broader) to see logs on stderr.

In the TUI, set `GROK_LOG_FILE` to an absolute path to write logs to that file:

```bash
GROK_LOG_FILE=/tmp/bladerunner.log RUST_LOG=debug bladerunner
tail -f /tmp/bladerunner.log
```

`GROK_LOG_FILE` is treated as a literal file path. A relative value such as `1` writes a file named `1` in the current directory.

In headless mode, logs go to stderr. Redirect them to a file:

```bash
RUST_LOG=debug bladerunner -p "hello" 2> /tmp/bladerunner.log
```

### Common log messages

| Log message | What it means |
|-------------|---------------|
| `auth: running external auth provider (headless refresh)` / `(interactive login)` | Bladerunner is running your binary, and on which contract |
| `auth: external auth provider returned fresh token` | Bladerunner parsed and stored the token |
| `auth: external auth provider failed` | Binary exited non-zero or stdout was empty |
| `auth: external auth provider timed out (likely needs interactive auth), killing` | Binary did not exit before the timeout and was killed |
| `auth: failed to start external auth provider` | Command could not be spawned (binary not found) |

### Common fixes

- **"Authentication failed"** -- Run `bladerunner logout` to clear cached credentials, then `bladerunner login` to sign in again.
- **Token expires too quickly** -- Set `auth_token_ttl` or return `expires_in` in your auth provider's JSON output.
- **OIDC redirect fails** -- Ensure your IdP allows loopback redirect URIs (`http://127.0.0.1/callback`).
- **External auth provider not found** -- Check that the `auth_provider_command` path is correct and the binary is executable.
