# Blade Runner

Blade Runner is a terminal coding-agent harness for OpenAI Codex and Palantir
AIP (Foundry). It provides tool execution, workspace access, sessions, MCP,
skills, and an ACP interface for compatible clients.

```text
Blade Runner terminal / ACP client
                |
       Blade Runner runtime
           /           \
    OpenAI Codex    Palantir AIP
```

Blade Runner does not bundle or manage a model inference engine. The normal
model catalog exposes Codex and AIP; inherited provider transports remain in
source for upstream compatibility and advanced configuration.

## Install

The binary beta supports **Apple Silicon macOS**. Intel macOS, Linux, Windows,
and a desktop application are not included in the published binary matrix.

```sh
curl -fsSL https://raw.githubusercontent.com/tginart/bladerunner/main/install.sh | sh
export PATH="$HOME/.bladerunner/bin:$PATH"
bladerunner --version
```

The installer verifies the release's SHA-256 checksums. Public downloads do not
require a GitHub token. You can also download the archive and `SHA256SUMS` from
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

Browser OAuth is also supported. A Foundry administrator registers a public
OAuth client once per Foundry deployment; see [Foundry setup](docs/foundry-app.md).
Model availability and usage limits depend on your provider account. No Foundry
hostname, OAuth client ID, or credential is shipped with Blade Runner.

## Configuration and data

State lives under `~/.bladerunner`, or `BLADERUNNER_HOME` when set. User settings
in `config.toml` override the bundled defaults. Credentials are stored separately
under `credentials/`. See [privacy and permissions](docs/privacy.md) and
[updates and removal](docs/updates.md).

An ACP client can launch the harness with:

```sh
bladerunner agent stdio
```

`agent serve` is an agent WebSocket transport. It does not serve model inference.

## Build from source

Install the Rust toolchain specified in `rust-toolchain.toml`, a C/C++ compiler,
CMake, and [DotSlash](https://dotslash-cli.com/). On macOS, install Xcode Command
Line Tools. DotSlash resolves the checksum-pinned protobuf compiler in `bin/protoc`.

```sh
GROK_VERSION="$(cat VERSION)" cargo build -p xai-grok-pager-bin --features community-build --release --locked
target/release/bladerunner --version
```

The `GROK_VERSION` build variable and `xai-*` crate names are inherited runtime
identifiers. The installed executable and local state belong to Blade Runner.
See [contributing](CONTRIBUTING.md) for verification commands and support limits.

## Source and license

This repository contains a reviewed source snapshot with fresh public history.
`PUBLIC_MANIFEST.json` records the exact source files, executable modes, sizes,
and SHA-256 hashes. Public CI verifies an archived snapshot in an isolated copy.

Blade Runner is Apache-2.0. It derives from Hyper and Grok Build; see
[provenance](PROVENANCE.md), [LICENSE](LICENSE), [NOTICE](NOTICE), and
[third-party notices](THIRD-PARTY-NOTICES). It is independently maintained and
is not an official OpenAI, Palantir, or xAI product.
