# Models, Providers, and Scoped Selection

Bladerunner separates the harness from the model provider. The community build has two managed authentication adapters—OpenAI Codex and xAI/Grok—and also accepts user-defined compatible model endpoints.

## Quick reference

| Task | Command or setting |
|------|--------------------|
| List or switch models | `/model` or `/model <id>` |
| Sign in to Codex | `/login codex` |
| Sign in to xAI/Grok | `/login grok` |
| Define a compatible endpoint | `[model.<name>]` in `~/.bladerunner/config.toml` |
| Set the default | `[models] default = "…"` |
| Restrict usable models | `[models] allowed_models = […]` |
| Set the fast-cycle shortlist | `[models] enabled_models = […]` |
| Hide picker entries | `[models] hidden_models = […]` |
| Manage the shortlist | `/scoped-models` |

Model identifiers are catalog keys or routing slugs. Use the identifiers shown by `/model`; do not assume that a provider's marketing name is its runtime slug.

## Community provider policy

The managed community surface includes:

- **OpenAI Codex** through ChatGPT subscription authentication.
- **xAI/Grok** through xAI OAuth or `XAI_API_KEY`.
- **User-defined compatible endpoints** configured explicitly by the user.

Other provider implementations may remain compiled for upstream synchronization or compatibility, but the community build does not advertise a giant managed-provider catalog. A provider is not part of Bladerunner's public surface merely because implementation code exists in the repository.

## Custom compatible endpoints

Use a `[model.<name>]` section for a model server you control or intentionally selected:

```toml
# ~/.bladerunner/config.toml
[model.local]
model = "my-model"
base_url = "http://127.0.0.1:11434/v1"
api_key = "local"
context_window = 32768
```

The endpoint must implement a wire protocol supported by the configured backend. See [Custom Models](11-custom-models.md) for the complete schema.

## Availability and visibility

These settings solve different problems:

| Setting | Effect |
|---------|--------|
| `allowed_models` | Hard allowlist. Nonmatching models cannot be selected for chat or subagents. |
| `enabled_models` | Soft shortlist used for fast cycling. It does not authorize a model. |
| `hidden_models` | Hides matching entries from the picker while retaining explicit selection where otherwise allowed. |
| `disabled_models` | Removes matching entries from the resolved catalog. |

An empty or absent `allowed_models` means no additional allowlist restriction. Exact names and glob patterns are supported.

```toml
[models]
default = "openai-codex/gpt-5"
allowed_models = ["openai-codex/*", "grok-*"]
enabled_models = ["openai-codex/*", "grok-*"]
hidden_models = ["*-legacy"]
```

## Scoped model cycling

Scoped models are a convenience shortlist, not a permission boundary.

- `Alt+]`: next shortlisted model
- `Alt+[`: previous shortlisted model
- `/scoped-models`: inspect or change the shortlist
- `/model`: open the full permitted picker

```text
/scoped-models add openai-codex/*
/scoped-models add grok-*
/scoped-models remove *-legacy
/scoped-models clear
```

When the shortlist is empty, cycling walks all currently usable models. `allowed_models` still applies.

## Subagent models

Subagents inherit the parent model by default. An explicitly requested model must pass the same catalog, authentication, and `allowed_models` checks as the parent session. The community task-tool prompt intentionally does not inject every available API slug.

## Related guides

- [Authentication](02-authentication.md)
- [Custom Models](11-custom-models.md)
- [OpenAI Codex](28-openai-codex.md)
- [Configuration](05-configuration.md)
