# Security policy

## Supported versions

Cortex is pre-1.0. Security fixes land on the latest release only.

| Version | Supported |
|---------|-----------|
| 0.3.x   | yes       |
| < 0.3   | no        |

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Use GitHub's private reporting: **Security → Report a vulnerability** on
[github.com/AstroQuestStudio/cortex](https://github.com/AstroQuestStudio/cortex/security/advisories/new).
If that is not possible, write to **astroqueststudio@gmail.com** with "cortex security" in the subject.

Include the Cortex version (`cortex --version`), your OS, and the smallest reproduction you can.
We acknowledge within 72 hours and aim to ship a fix or a mitigation within 30 days for
confirmed issues. We credit reporters in the release notes unless they prefer otherwise.

## Security model (what Cortex does and does not do)

- **Local only.** Indexing, search and every agent tool run on your machine. Cortex sends no
  code, query or telemetry anywhere. Its data lives in `~/.cortex/` (one atlas per project).
- **Network access** happens only when you ask for it: `cortex docs add|batch` downloads the
  documentation sites you list, and `cortex infra` opens read-only SSH sessions to the hosts in
  the `.env` file you pass (it never copies secrets from that file into its output).
- **The MCP server** (`cortex mcp`) speaks JSON-RPC over stdio to the client that started it; it
  opens no port. `cortex viewer` serves the 3D viewer on `127.0.0.1` only.
- **File access**: tools read only files that are in the atlas of a project you indexed
  (`read` resolves its input to an indexed file first; an unknown path is an error, not a read).
  Indexing honours `.gitignore`, `.ignore` and git's exclude files.

Topics we consider in scope: path traversal out of an indexed root, code execution through a
crafted repository or documentation page, crashes on malicious input that a normal repository
could contain, and anything that would leak data off the machine.
