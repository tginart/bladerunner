# Privacy and permissions

Bladerunner sends your messages, file content it reads, and tool results to
your selected model provider. Codex requests go to OpenAI; AIP requests go to
your Foundry. Your provider's privacy policies and access controls apply.
Only give Bladerunner access to data you can share with that provider.

Sessions, configuration, credentials, and logs are local under `~/.bladerunner`
unless `BLADERUNNER_HOME` is set. Saved credentials are in `credentials/`.
Do not share this directory. Session files and logs can contain messages,
source code, and tool output. Review and redact them before sharing.

Product telemetry is off by default. You or your administrator can enable it.
Review what data it sends and where before enabling it.

Tools, shell commands, plugins, hooks, extensions, and MCP servers can access
files and run commands with the permissions you grant. Review permission
requests before allowing execution. Use restrictive permissions and a separate
workspace for untrusted projects.

`bladerunner logout --openai` and `bladerunner logout --palantir-aip` remove the
corresponding saved sign-in. Revoke provider-side access separately when needed.
Removing local files does not delete data already submitted to a provider.
