# Security

Security fixes target the latest Bladerunner release. Report vulnerabilities
privately through [GitHub vulnerability reporting](https://github.com/tginart/bladerunner/security/advisories/new).
If that route is unavailable, contact the maintainer through the
[public GitHub profile](https://github.com/tginart) and request a private channel
before sharing details. Do not put tokens, private code, or exploit details in a
public issue.

Bladerunner can read files and run commands with the permissions you grant.
Use a separate workspace and restrictive permissions for untrusted code,
skills, hooks, extensions, and MCP servers. Provider requests can contain source
code and tool output. Review [privacy and permissions](docs/privacy.md).
