# Periscope

An MCP server that brokers to a real Neovim running your actual diver
config, so MCP clients (agents) query the same language servers,
formatter registry, linter registry, and DAP module tree you edit
with — one configuration source of truth, no duplicate servers, no
translated config files.

Periscope is a single Rust binary. It speaks MCP over stdio
(NDJSON JSON-RPC) to the client and msgpack-RPC to Neovim.

## How it runs

- **Managed mode** (default): spawns its own
  `nvim --headless --listen <tmp socket>` with cwd set to the
  workspace. Your config loads exactly as it does for you; the
  broker never touches your interactive editor. Project-local config
  execution is refused (`--cmd "set noexrc"`).
- **Attach mode** (`--attach <socket>`): connects to a Neovim you
  already have running (anything started with `--listen`). The
  broker never signals or closes an editor it did not spawn.

Tools are small Lua programs executed inside the editor via
`nvim_exec_lua`. They use native APIs only (`vim.lsp`, `vim.diagnostic`,
runtime files) and inherit every per-server setting in your `lsp/*.lua`
files — including deliberate choices like rust-analyzer's native
diagnostics being disabled in favour of bacon-ls.

## Tools (v0.1, read-only)

| Tool | What it returns |
| --- | --- |
| `broker_status` | mode, Neovim version, cwd, config root |
| `get_definition` | merged definition locations across attached clients |
| `get_diagnostics` | buffer diagnostics from `vim.diagnostic` (LSP + linter pipelines), with the producing `source` named on every entry |
| `get_document_symbols` | flattened symbol outline with containers |
| `get_hover` | hover text at a position |
| `get_references` | merged reference locations across attached clients |
| `list_dap` | DAP modules in the config tree plus adapters/configurations registered in the session |
| `list_formatters` | formatter modules in the config tree |
| `list_linters` | linter modules in the config tree (underscore-prefixed modules reported as internal) |
| `list_servers` | LSP servers configured in the tree, plus servers attached to any open buffer |

Positions are 0-based; `character` counts UTF-16 code units (the LSP
convention). LSP request tools poll inside the editor while servers
report in-flight progress, so a cold rust-analyzer returns its real
answer (~10 s on a small project) instead of a fast, empty one.

<details>
<summary>Usage</summary>

```
periscope [--workspace <dir>] [--attach <socket>] [--nvim-bin <path>]
```

- `--workspace` defaults to the current directory and is where the
  managed editor starts.
- Register it where you register MCP servers; in diver that is a
  `declared_servers` entry in `lua/ai/mcp/declared_servers.lua`:

```lua
periscope = {
    args = { '--workspace', '/path/to/project' },
    cmd = '/home/phaedrus/.local/bin/periscope',
    kind = 'stdio',
    name = 'periscope',
    source = 'local',
    timeout = 5000,
    tools = 'all',
},
```

Adapt `cmd`/`args` to the installation; drop the `--workspace` arg
entry to use the client's working directory, or point it at a
running editor with `--attach <socket>` instead.

</details>

<details>
<summary>Budgets and bounds</summary>

Every wait and every list is bounded: editor attach 20 s, LSP
request phase 30 s (progress-gated polling), diagnostics settle 5 s,
connect deadline 10 s, VimEnter readiness 20 s. Result caps:
500 diagnostics locations per call, 1,000 document symbols (depth 6),
500 definition/reference locations, 4 MiB per MCP input line.
Errors from the editor are forwarded capped at 512 characters.
Tool failures are in-band MCP `isError` results, never protocol
errors.

</details>

<details>
<summary>Security posture</summary>

stdio transport only — nothing listens on a network port. The
managed editor's socket lives in a private temp path and is removed
on shutdown; the broker reaps its child. v0.1 is read-only: no tool
writes a file, formats, renames, or starts a debug session.

</details>

<details>
<summary>Roadmap</summary>

- P2: formatter and linter *execution* through the config's own
  adapters.
- P3: write tools (rename, code actions, format) behind an explicit
  enable flag.
- P4: SCIP query tools over `index.scip` (native Rust reader).
- P5: DAP session driving.

</details>
