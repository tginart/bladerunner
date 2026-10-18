# Bladerunner

Bladerunner is a terminal coding agent for OpenAI Codex and Palantir AIP
(Foundry), forked from [Grok Build](https://github.com/xai-org/grok-build).
It edits files, runs commands, and works with MCP servers and skills.

## Install

Prebuilt beta releases support **Apple Silicon macOS**.

```sh
curl -fsSL https://raw.githubusercontent.com/tginart/bladerunner/main/install.sh | sh
export PATH="$HOME/.bladerunner/bin:$PATH"
bladerunner --version
```

The installer verifies SHA-256 checksums. You can also download releases from
[GitHub Releases](https://github.com/tginart/bladerunner/releases).

## Sign in and start

For Codex, sign in with your ChatGPT account:

```sh
bladerunner login --openai
bladerunner
```

For AIP, provide your own Foundry hostname and personal token:

```sh
bladerunner login --palantir-aip --token
bladerunner models
bladerunner
```

For browser sign-in, a Foundry administrator must register an OAuth client
once. See [Foundry setup](docs/foundry-app.md). Your provider account determines
available models and usage limits.

## Configuration and data

Sessions and settings live under `~/.bladerunner`, or `BLADERUNNER_HOME` when set.
Settings in `config.toml` override the bundled defaults. Credentials are stored separately
under `credentials/`. See [privacy and permissions](docs/privacy.md) and
[updates and removal](docs/updates.md).

To connect from an ACP-compatible editor:

```sh
bladerunner agent stdio
```

For source builds and contributions, see [Contributing](CONTRIBUTING.md).

## License

Licensed under Apache-2.0. See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[third-party notices](THIRD-PARTY-NOTICES). Bladerunner is independently
maintained and is not an official OpenAI, Palantir, or xAI product.
