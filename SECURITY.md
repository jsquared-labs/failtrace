# Security policy

Failtrace is alpha software. It deliberately captures test and type-checker output, so its run artifacts can contain sensitive information even when Failtrace itself is working correctly.

## Supported versions

Security fixes are made on a best-effort basis for the latest published `0.1.x` alpha release. Older alpha builds are not supported or guaranteed to receive backports. The unreleased `main` branch may contain fixes that have not yet shipped.

## Report a vulnerability privately

Do not open a public issue or discussion for a suspected vulnerability. Use [GitHub's private vulnerability reporting form](https://github.com/jsquared-labs/failtrace/security/advisories/new) so maintainers can investigate before disclosure.

Please include:

- the affected Failtrace version and platform;
- the security impact and the conditions needed to trigger it;
- minimal reproduction steps or a synthetic reproducer; and
- sanitized excerpts only when they are necessary to understand the issue.

Response and remediation are best effort; this alpha project does not yet promise a response-time or disclosure-time SLA. Please coordinate public disclosure through the private advisory.

## Treat run artifacts as sensitive

Each run directory contains `log`, `meta.json`, and `output.json`. Depending on the wrapped command, these files may contain source excerpts, absolute paths, test data, environment-derived diagnostics, a full shell-joined command, or credentials passed on the command line.

Never post an unredacted run directory or any of its files in a public issue, pull request, gist, or chat. Before sharing any excerpt, inspect all three files, remove secrets and private data, and verify that the remaining fragment still reproduces the parser behavior. Prefer a new synthetic run over redacting a real private run.

If the problem is a normal adapter bug rather than a vulnerability, follow the safe reporting recipe in [CONTRIBUTING.md](CONTRIBUTING.md#report-an-adapter-bug-safely).
