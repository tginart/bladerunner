# Monitoring Usage with OpenTelemetry

Bladerunner can export content-free operational metrics and events to an OpenTelemetry collector you control. This is optional and independent of model-provider authentication.

## Interactive usage

Run `/usage` (alias `/cost`) to inspect the current context window and cumulative session token usage. These local measurements work with Codex, Grok, and compatible custom models.

If an authenticated provider exposes account allowance or billing data, the same modal may include an additional provider-specific tab. Bladerunner keeps that adapter data separate from the provider-neutral context and session totals. External-auth deployments and providers without an account-usage API simply omit the account tab.

## Privacy boundary

The external stream is designed for operational measurements such as token counts, latency, session counts, and tool outcomes. It must not include prompts, responses, file contents, credentials, email addresses, or subscription details.

Collector credentials come from standard OpenTelemetry environment variables and are never written to Bladerunner configuration files.

## Quick start

```bash
export BLADERUNNER_EXTERNAL_OTEL=1
export OTEL_EXPORTER_OTLP_ENDPOINT="https://otel.example.com"
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer ..."
bladerunner
```

`GROK_EXTERNAL_OTEL` remains a deprecated alias during migration. If both spellings are set, `BLADERUNNER_EXTERNAL_OTEL` wins.

## Configuration

```toml
# ~/.bladerunner/config.toml
[telemetry]
otel_enabled = true
otel_metrics = true
otel_logs = true
otel_service_name = "bladerunner"
```

The external collector is not enabled merely because a provider login exists. You must configure both the Bladerunner switch and a destination signal or endpoint.

## Standard OpenTelemetry variables

Bladerunner honors the normal OTEL exporter variables, including:

- `OTEL_EXPORTER_OTLP_ENDPOINT`
- `OTEL_EXPORTER_OTLP_HEADERS`
- `OTEL_EXPORTER_OTLP_PROTOCOL`
- `OTEL_SERVICE_NAME`
- `OTEL_RESOURCE_ATTRIBUTES`

Use the standard variables for collector transport. Use `[telemetry] otel_*` only for Bladerunner's signal selection and defaults.

## Expected signals

Depending on configuration and runtime support, the exporter may report:

- Input, output, cached, and reasoning token counts
- Model and provider identifiers
- Request latency and error counts
- Session and turn counts
- Tool invocation, completion, denial, and failure counts
- Subagent and background-task lifecycle counts

These are operational dimensions, not conversation content.

## Troubleshooting

1. Confirm `BLADERUNNER_EXTERNAL_OTEL=1` or `[telemetry] otel_enabled = true`.
2. Confirm at least one signal is enabled.
3. Confirm the OTLP endpoint and protocol match the collector.
4. Run with `RUST_LOG=debug` and `BLADERUNNER_LOG_FILE=/tmp/bladerunner.log`.
5. Inspect collector logs for authentication or schema errors.

Failure to reach a collector must not block coding sessions. Export failures are reported through diagnostics and logs.
