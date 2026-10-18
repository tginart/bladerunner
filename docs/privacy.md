# Privacy and permissions

Bladerunner sends prompts, selected workspace context, and tool results to the
configured model provider. Codex requests go to OpenAI; AIP requests go to the
Foundry hostname configured by the user. Their account policies, access controls,
and usage limits apply. Do not give the harness access to data you cannot send
to that provider.

Sessions, configuration, credentials, and logs are local under `~/.bladerunner`
unless `BLADERUNNER_HOME` is set. Credentials are stored in owner-restricted
files; do not copy this directory into a public repository. Local session and
diagnostic files can contain prompts, source code, and tool output. Review and
redact them before sharing. Logging is not a guarantee of secret removal.

Product telemetry defaults to disabled. No telemetry collector, Mixpanel token,
or Sentry DSN is bundled. Users or administrators can explicitly configure
telemetry and external OpenTelemetry exporters. Their configuration determines
the destination and enabled content; inspect it before enabling export.

Tools, shell commands, plugins, hooks, extensions, and MCP servers can access
resources permitted by the runtime and operating system. Review permissions and
trust settings before allowing execution. Prefer restrictive permissions and a
disposable workspace for untrusted projects.

`bladerunner logout --openai` and `bladerunner logout --palantir-aip` remove the
corresponding saved sign-in. Revoke provider-side access separately when needed.
Removing local files does not delete data already submitted to a provider.
