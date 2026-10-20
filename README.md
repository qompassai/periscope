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

## Tools

Read-only intelligence (`broker_status`, `get_*`, `list_*`) plus two
execution tools (`run_format`, `run_lint`) that run the config's own
pipelines inside the editor. Formatting is applied to the brokered
editor's buffer and returned as text — the file on disk is never
written (`written` is always `false`; write-to-disk tools are a
later phase behind an explicit enable flag). Linting only reads.
Four `attention_*` tools serve a joint-attention coordination board
held in the server process (the MATE protocol, vendored from
phlow-attention): agents publish an ordered focus, read each other's,
and ask for an alignment score or a coordination decision. The board
never touches the editor or the filesystem, and a publish is bound to
the identity the client presented at `initialize`.

| Tool | What it returns |
| --- | --- |
| `attention_alignment` | Alignment between your published focus and a partner agent's, measured only inside one saliency snapshot frame (Jaccard overlap, shared targets, shared top); refused across frames |
| `attention_decide` | Coordination decision against a partner agent's focus: parallel (no contention), serialize (same top target — the named holder proceeds, you defer), or reject (stale frame or invalid focus — re-snapshot, never guess) |
| `attention_focus` | A partner agent's currently advertised focus (ordered salient targets + the snapshot hash it was computed from) from the coordination board |
| `attention_publish` | Publish your focus to the coordination board: a focus document `{agent_id, snapshot_hash, seq, targets[]}` (at most 8 targets); the agent_id must equal your client identity and sequences must strictly advance |
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
| `run_format` | Format a file with the config's formatter pipeline (`require('formatters')`); optional explicit formatter names and LSP mode (`fallback` / `never` / `only`); returns status, `changed`, the steps that ran, and the formatted text |
| `run_lint` | Lint a file with the config's linter runner (`require('linters')`); optional explicit linter names, otherwise the filetype's configured set; returns per-linter status, exit code, and diagnostics (each naming its linter in `source`) |

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
on shutdown; the broker reaps its child. No tool writes a file to
disk, renames, or starts a debug session: formatting changes only the
brokered editor's buffer and is returned as text, and the attention
board is in-process memory that is dropped when the server exits.

</details>

<details>
<summary>Roadmap</summary>

- P2 (done): formatter and linter *execution* through the config's
  own pipelines (`run_format`, `run_lint`).
- P3: write tools (rename, code actions, format-and-write) behind
  an explicit enable flag.
- P4: SCIP query tools over `index.scip` (native Rust reader).
- P5: DAP session driving.

</details>
