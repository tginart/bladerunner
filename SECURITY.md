# Security

Security fixes target the latest Blade Runner release. Report vulnerabilities
privately through [GitHub vulnerability reporting](https://github.com/tginart/bladerunner/security/advisories/new).
If that route is unavailable, contact the maintainer through the
[public GitHub profile](https://github.com/tginart) and request a private channel
before sharing details. Do not put tokens, private code, or exploit details in a
public issue.

The harness can read workspace files and execute tools with the permissions you
grant. Use a separate workspace and restrictive permissions for untrusted code,
skills, hooks, extensions, and MCP servers. Provider requests can contain source
code and tool output. Review [privacy and permissions](docs/privacy.md).

The source-integrity and credential-pattern checks are publication safeguards;
passing them does not establish that the software has no vulnerabilities.
