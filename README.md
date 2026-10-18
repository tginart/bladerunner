# Bladerunner

Bladerunner is a terminal coding agent for OpenAI Codex and Palantir AIP
(Foundry). It edits files, runs commands, and works with MCP servers and skills.

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

Edit `~/.bladerunner/config.toml` to change your settings. Saved sign-ins and
sessions also live under `~/.bladerunner`. Set `BLADERUNNER_HOME` to use a
different directory. See [privacy and permissions](docs/privacy.md) and
[updates and removal](docs/updates.md).

To connect from an editor that supports the Agent Client Protocol (ACP):

```sh
bladerunner agent stdio
```

For source builds and contributions, see [Contributing](CONTRIBUTING.md).

## License

Bladerunner is an independently maintained fork of
[Grok Build](https://github.com/xai-org/grok-build), licensed under Apache-2.0.
See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[third-party notices](THIRD-PARTY-NOTICES). It is not an official OpenAI,
Palantir, or xAI product.
