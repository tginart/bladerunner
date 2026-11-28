# Bladerunner

Bladerunner is a fork of the open source
[Grok Build](https://github.com/xai-org/grok-build) terminal agent.

It supports OpenAI Codex and Palantir AIP (Foundry). It edits files, runs
commands, and works with MCP servers and skills.

## Install

Prebuilt beta releases support **Apple Silicon macOS** and **x86_64 Linux** (glibc 2.17+).

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

## Autonomous goals

`/goal <objective>` automatically retries temporary network, timeout, rate-limit,
and server failures. It permits eight consecutive retries, with exponential
backoff from 15 seconds to five minutes and 20% jitter. A server `Retry-After`
value sets the minimum wait and can exceed the backoff cap. A successful goal
round resets the retry count. Set `GROK_GOAL_MAX_CONSECUTIVE_RETRIES` before launch to
change the limit; `0` disables these retries. The sampler also has its own
request retry budget, so total recovery time includes both layers.

`/goal pause`, `/goal stop`, and Ctrl+C interrupt a retry wait. `/goal resume`
starts a fresh retry budget after a pause. Authentication, configuration, and
invalid-request errors pause the goal immediately.

## Hubs

Agent threads can talk to each other in a hub, with channels and DMs. In one
session, run `/hub new api`. In another, run `/hub join api`. Their agents can
now post with the `message_agents` tool, and a DM or an `@handle` mention wakes an
idle agent. Your own channel posts wake every agent, as `@here` does; run
`/hub here off` to stop that in a hub. `/fork` keeps the hub; n-shot shots do
not join it. Run `/hub` for status and the other commands. Hubs live under `~/.bladerunner/hubs`.

## License

Bladerunner is independently maintained and licensed under Apache-2.0.
See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[third-party notices](THIRD-PARTY-NOTICES). It is not an official OpenAI,
Palantir, or xAI product.
