# astroquest-cortex (npm)

npm distribution of **[Cortex](https://github.com/AstroQuestStudio/cortex)**, the code-context
engine for AI agents, by [AstroQuest](https://astroquest.fr).

This package contains no JavaScript engine: it downloads the native `cortex` binary of the same
version from the GitHub release (SHA-256 checked against `SHA256SUMS.txt`) and runs it.

```sh
npx -y astroquest-cortex index . --name MyProject
npx -y astroquest-cortex find "where are sessions signed"
npm install -g astroquest-cortex     # then: cortex --version
```

MCP client configuration without a global install:

```json
{ "mcpServers": { "cortex": { "command": "npx", "args": ["-y", "astroquest-cortex", "mcp"] } } }
```

Prebuilt binaries: Linux x86_64, macOS arm64 and x86_64, Windows x86_64. Elsewhere:
`cargo install astroquest-cortex`. `CORTEX_BINARY=/path/to/cortex` uses an existing binary.

PolyForm Shield 1.0.0 license (source-available, free to use; versions up to 0.3.0 were MIT). See LICENSE.
