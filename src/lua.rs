//! Lua programs executed inside the brokered Neovim instance.
//!
//! Each constant is a self-contained chunk run through `nvim_exec_lua`
//! with positional arguments in `...`. Snippets are contractually
//! required to return string-keyed tables of plain data (no functions,
//! no userdata), keep every wait bounded by a caller-supplied budget,
//! and sort every list they return so output is deterministic.

/// Shared prelude for file-oriented snippets: load a buffer without
/// displaying it, wait (bounded) for a language server to attach, and
/// list the servers attached to a buffer.
const PRELUDE: &str = r"
local function ensure_buffer(path)
  local bufnr = vim.fn.bufadd(path)
  if not vim.api.nvim_buf_is_loaded(bufnr) then
    vim.fn.bufload(bufnr)
  end
  return bufnr
end
local function wait_attach(bufnr, budget_ms)
  local function ready()
    for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr })) do
      if client.initialized then
        return true
      end
    end
    return false
  end
  if ready() then
    return true
  end
  vim.wait(budget_ms, ready, 100)
  return #vim.lsp.get_clients({ bufnr = bufnr }) > 0
end
local function attached_names(bufnr)
  local names = {}
  for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr })) do
    names[#names + 1] = client.name
  end
  table.sort(names)
  return names
end
local function any_client_busy(bufnr)
  for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr })) do
    local ok, busy = pcall(function()
      local active = {}
      for _, entry in pairs(client.progress or {}) do
        if entry.token and entry.value and entry.value.kind then
          if entry.value.kind == 'end' then
            active[tostring(entry.token)] = nil
          else
            active[tostring(entry.token)] = true
          end
        end
      end
      return next(active) ~= nil
    end)
    if ok and busy then
      return true
    end
  end
  return false
end
";

/// Broker status: Neovim version, config root, working directory.
/// Arguments: none.
pub const STATUS: &str = r"
local version = vim.version()
return {
  config_root = vim.fn.stdpath('config'),
  cwd = vim.fn.getcwd(),
  nvim_version = string.format('%d.%d.%d', version.major, version.minor, version.patch),
  vim_starting = vim.fn.has('vim_starting'),
}
";

/// Language servers: every server the config defines (runtime files
/// under `lsp/`) and every client currently active in the editor.
/// Arguments: none.
pub const SERVERS: &str = r"
local configured, seen = {}, {}
for _, file in ipairs(vim.api.nvim_get_runtime_file('lsp/*.lua', true)) do
  local name = file:match('([^/]+)%.lua$')
  if name and not seen[name] then
    seen[name] = true
    configured[#configured + 1] = name
  end
end
table.sort(configured)
local attached = {}
for _, client in ipairs(vim.lsp.get_clients()) do
  attached[#attached + 1] = {
    id = client.id,
    name = client.name,
    root_dir = client.config.root_dir or '',
  }
end
table.sort(attached, function(a, b) return a.name < b.name end)
return { attached = attached, configured = configured, configured_count = #configured }
";

/// Formatter registry, read from the runtime files that define it.
/// Arguments: none.
pub const FORMATTERS: &str = r"
local infra = { catalog = true, init = true }
local names = {}
for _, file in ipairs(vim.api.nvim_get_runtime_file('lua/formatters/*.lua', true)) do
  local name = file:match('([^/]+)%.lua$')
  if name and not infra[name] then
    names[#names + 1] = name
  end
end
table.sort(names)
return { count = #names, formatters = names, source = 'runtime files lua/formatters/*.lua' }
";

/// Linter registry. Underscore-prefixed modules are internal helpers
/// and are reported separately from selectable linters.
/// Arguments: none.
pub const LINTERS: &str = r"
local infra = { init = true }
local internal, names = {}, {}
for _, file in ipairs(vim.api.nvim_get_runtime_file('lua/linters/*.lua', true)) do
  local name = file:match('([^/]+)%.lua$')
  if name and not infra[name] then
    if name:sub(1, 1) == '_' then
      internal[#internal + 1] = name
    else
      names[#names + 1] = name
    end
  end
end
table.sort(internal)
table.sort(names)
return {
  count = #names,
  internal = internal,
  linters = names,
  source = 'runtime files lua/linters/*.lua',
}
";

/// Debug setup: the per-language dap modules the config ships, plus
/// the adapters and launch configurations actually registered with
/// nvim-dap right now (empty until the dap module has loaded).
/// Arguments: none.
pub const DAP: &str = r"
local infra = { _cmds = true, async = true, backend = true, init = true }
local modules = {}
for _, file in ipairs(vim.api.nvim_get_runtime_file('lua/dap/*.lua', true)) do
  local name = file:match('([^/]+)%.lua$')
  if name and not infra[name] then
    modules[#modules + 1] = name
  end
end
table.sort(modules)
local adapters, configurations, dap_loaded = {}, {}, false
local ok, dap = pcall(require, 'dap')
if ok and type(dap) == 'table' then
  dap_loaded = true
  for name in pairs(dap.adapters or {}) do
    adapters[#adapters + 1] = name
  end
  table.sort(adapters)
  for filetype, configs in pairs(dap.configurations or {}) do
    local names = {}
    for _, entry in ipairs(configs) do
      names[#names + 1] = entry.name or entry.type or '?'
    end
    configurations[#configurations + 1] = { configurations = names, filetype = filetype }
  end
  table.sort(configurations, function(a, b) return a.filetype < b.filetype end)
end
return {
  adapters_registered = adapters,
  configurations = configurations,
  dap_loaded = dap_loaded,
  modules = modules,
}
";

/// Diagnostics for one file, from every source the editor aggregates
/// (LSP servers and the linter pipeline alike), each entry carrying
/// its producing tool in `source`.
/// Arguments: path, attach budget ms, settle budget ms.
pub fn diagnostics() -> String {
    format!(
        "{PRELUDE}{body}",
        body = r"
local path, attach_ms, settle_ms = ...
local bufnr = ensure_buffer(path)
local attached = wait_attach(bufnr, attach_ms)
local last_count, stable_polls = -1, 0
local deadline = vim.uv.now() + settle_ms
while vim.uv.now() < deadline do
  local count = #vim.diagnostic.get(bufnr)
  if count == last_count then
    stable_polls = stable_polls + 1
  else
    stable_polls = 0
  end
  if stable_polls >= 2 then
    break
  end
  last_count = count
  vim.wait(120)
end
local severity_names = { 'ERROR', 'WARN', 'INFO', 'HINT' }
local out, truncated = {}, false
for index, entry in ipairs(vim.diagnostic.get(bufnr)) do
  if index > 500 then
    truncated = true
    break
  end
  out[#out + 1] = {
    character = entry.col,
    code = entry.code ~= nil and tostring(entry.code) or nil,
    end_character = entry.end_col,
    end_line = entry.end_lnum,
    line = entry.lnum,
    message = entry.message,
    severity = severity_names[entry.severity] or 'UNKNOWN',
    source = entry.source or '',
  }
end
return {
  attached_servers = attached_names(bufnr),
  diagnostics = out,
  file = path,
  server_attached = attached,
  settled = stable_polls >= 2,
  truncated = truncated,
}
"
    )
}

/// Hover text at a position.
/// Arguments: path, line, character, attach budget ms, request budget ms.
pub fn hover() -> String {
    format!(
        "{PRELUDE}{body}",
        body = r"
local path, line, character, attach_ms, request_ms = ...
local bufnr = ensure_buffer(path)
if not wait_attach(bufnr, attach_ms) then
  return { attached_servers = {}, found = false, note = 'no language server attached within budget', text = '' }
end
local params = {
  position = { character = character, line = line },
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
}
local function extract(responses)
  if not responses then
    return nil
  end
  for _, response in pairs(responses) do
    local contents = response.result and response.result.contents
    if contents then
      if type(contents) == 'string' then
        return contents
      elseif contents.value then
        return contents.value
      elseif vim.islist(contents) then
        local parts = {}
        for _, part in ipairs(contents) do
          parts[#parts + 1] = part.value or part
        end
        return table.concat(parts, '\n\n')
      end
    end
  end
  return nil
end
local text = nil
local deadline = vim.uv.now() + request_ms
while true do
  text = extract(vim.lsp.buf_request_sync(bufnr, 'textDocument/hover', params, 3000))
  if (text ~= nil and text ~= '') or vim.uv.now() >= deadline then
    break
  end
  if not any_client_busy(bufnr) then
    vim.wait(400)
    text = extract(vim.lsp.buf_request_sync(bufnr, 'textDocument/hover', params, 3000)) or text
    break
  end
  vim.wait(400)
end
return { attached_servers = attached_names(bufnr), found = text ~= nil and text ~= '', text = text or '' }
"
    )
}

/// Location-list normalizer shared by the definition and references
/// snippets: accepts a Location, a LocationLink, or a list of either.
const NORMALIZE_LOCATIONS: &str = r"
local function normalize_locations(result, cap)
  local locations, truncated = {}, false
  local function push(uri, range)
    if #locations >= cap then
      truncated = true
      return
    end
    locations[#locations + 1] = {
      character = range.start.character,
      line = range.start.line,
      path = vim.uri_to_fname(uri),
    }
  end
  local function handle(item)
    if item.uri and item.range then
      push(item.uri, item.range)
    elseif item.targetUri then
      push(item.targetUri, item.targetSelectionRange or item.targetRange)
    end
  end
  if result then
    if result.uri or result.targetUri then
      handle(result)
    else
      for _, item in ipairs(result) do
        handle(item)
      end
    end
  end
  return locations, truncated
end
";

/// Definitions of the symbol at a position, merged across clients.
/// Arguments: path, line, character, attach budget ms, request budget ms.
pub fn definition() -> String {
    format!(
        "{PRELUDE}{NORMALIZE_LOCATIONS}{body}",
        body = r"
local path, line, character, attach_ms, request_ms = ...
local bufnr = ensure_buffer(path)
if not wait_attach(bufnr, attach_ms) then
  return { attached_servers = {}, locations = {}, note = 'no language server attached within budget', truncated = false }
end
local params = {
  position = { character = character, line = line },
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
}
local function extract(responses)
  local locations, truncated = {}, false
  if responses then
    for _, response in pairs(responses) do
      local merged, cut = normalize_locations(response.result, 500)
      for _, location in ipairs(merged) do
        locations[#locations + 1] = location
      end
      truncated = truncated or cut
    end
  end
  return locations, truncated
end
local locations, truncated = {}, false
local deadline = vim.uv.now() + request_ms
while true do
  locations, truncated = extract(vim.lsp.buf_request_sync(bufnr, 'textDocument/definition', params, 3000))
  if #locations > 0 or vim.uv.now() >= deadline then
    break
  end
  if not any_client_busy(bufnr) then
    vim.wait(400)
    local retry_locations, retry_truncated =
      extract(vim.lsp.buf_request_sync(bufnr, 'textDocument/definition', params, 3000))
    if #retry_locations > 0 then
      locations, truncated = retry_locations, retry_truncated
    end
    break
  end
  vim.wait(400)
end
return { attached_servers = attached_names(bufnr), locations = locations, truncated = truncated }
"
    )
}

/// References to the symbol at a position, merged across clients.
/// Arguments: path, line, character, include declaration (boolean),
/// attach budget ms, request budget ms.
pub fn references() -> String {
    format!(
        "{PRELUDE}{NORMALIZE_LOCATIONS}{body}",
        body = r"
local path, line, character, include_declaration, attach_ms, request_ms = ...
local bufnr = ensure_buffer(path)
if not wait_attach(bufnr, attach_ms) then
  return { attached_servers = {}, locations = {}, note = 'no language server attached within budget', truncated = false }
end
local params = {
  context = { includeDeclaration = include_declaration },
  position = { character = character, line = line },
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
}
local function extract(responses)
  local locations, truncated = {}, false
  if responses then
    for _, response in pairs(responses) do
      local merged, cut = normalize_locations(response.result, 500)
      for _, location in ipairs(merged) do
        locations[#locations + 1] = location
      end
      truncated = truncated or cut
    end
  end
  return locations, truncated
end
local locations, truncated = {}, false
local deadline = vim.uv.now() + request_ms
while true do
  locations, truncated = extract(vim.lsp.buf_request_sync(bufnr, 'textDocument/references', params, 3000))
  if #locations > 0 or vim.uv.now() >= deadline then
    break
  end
  if not any_client_busy(bufnr) then
    vim.wait(400)
    local retry_locations, retry_truncated =
      extract(vim.lsp.buf_request_sync(bufnr, 'textDocument/references', params, 3000))
    if #retry_locations > 0 then
      locations, truncated = retry_locations, retry_truncated
    end
    break
  end
  vim.wait(400)
end
return { attached_servers = attached_names(bufnr), locations = locations, truncated = truncated }
"
    )
}

/// Document symbols, flattened with container names, depth- and
/// count-capped.
/// Arguments: path, attach budget ms, request budget ms.
pub fn document_symbols() -> String {
    format!(
        "{PRELUDE}{body}",
        body = r"
local path, attach_ms, request_ms = ...
local bufnr = ensure_buffer(path)
if not wait_attach(bufnr, attach_ms) then
  return { attached_servers = {}, note = 'no language server attached within budget', symbols = {}, truncated = false }
end
local params = { textDocument = { uri = vim.uri_from_bufnr(bufnr) } }
local function request_symbols()
  return vim.lsp.buf_request_sync(bufnr, 'textDocument/documentSymbol', params, 3000)
end
local responses = request_symbols()
local kind_names = {
  'File', 'Module', 'Namespace', 'Package', 'Class', 'Method', 'Property',
  'Field', 'Constructor', 'Enum', 'Interface', 'Function', 'Variable',
  'Constant', 'String', 'Number', 'Boolean', 'Array', 'Object', 'Key',
  'Null', 'EnumMember', 'Struct', 'Event', 'Operator', 'TypeParameter',
}
local out, truncated = {}, false
local function emit(name, kind, line, character, container)
  if #out >= 1000 then
    truncated = true
    return
  end
  out[#out + 1] = {
    character = character,
    container = container,
    kind = kind_names[kind] or 'Unknown',
    line = line,
    name = name,
  }
end
local function walk(symbols, container, depth)
  if depth > 6 then
    return
  end
  for _, symbol in ipairs(symbols) do
    if symbol.location then
      emit(symbol.name, symbol.kind, symbol.location.range.start.line,
        symbol.location.range.start.character, symbol.containerName or container)
    elseif symbol.range and symbol.selectionRange then
      emit(symbol.name, symbol.kind, symbol.selectionRange.start.line,
        symbol.selectionRange.start.character, container)
      if symbol.children then
        walk(symbol.children, symbol.name, depth + 1)
      end
    end
  end
end
local function collect(candidate)
  out, truncated = {}, false
  if candidate then
    for _, response in pairs(candidate) do
      if response.result then
        walk(response.result, nil, 1)
      end
    end
  end
end
collect(responses)
local deadline = vim.uv.now() + request_ms
while #out == 0 and vim.uv.now() < deadline and any_client_busy(bufnr) do
  vim.wait(400)
  collect(request_symbols())
end
if #out == 0 and vim.uv.now() < deadline then
  vim.wait(400)
  collect(request_symbols())
end
return { attached_servers = attached_names(bufnr), symbols = out, truncated = truncated }
"
    )
}

/// Format one file with the config's own formatter pipeline
/// (`require('formatters')`, the same entry point the editor's
/// format-on-save and `:Format`-style flows use). The pipeline
/// applies its result to the brokered editor's buffer; the file on
/// disk is never written by this tool (`written` is always false).
/// Arguments: path, formatter names (empty = the filetype's
/// configured pipeline), LSP mode ('fallback' | 'never' | 'only'),
/// attach grace ms, module timeout ms.
pub fn run_format() -> String {
    format!(
        "{PRELUDE}{body}",
        body = r"
local path, names, lsp_mode, attach_grace_ms, timeout_ms = ...
local bufnr = ensure_buffer(path)
local filetype = vim.bo[bufnr].filetype
local ok, formatters = pcall(require, 'formatters')
if not ok then
  return { error = 'formatters module failed to load: ' .. tostring(formatters), file = path, filetype = filetype, status = 'error', written = false }
end
if lsp_mode ~= 'never' and #names == 0 then
  wait_attach(bufnr, attach_grace_ms)
end
local opts = {
  async = false,
  bufnr = bufnr,
  notify = false,
  timeout_ms = timeout_ms,
}
if #names > 0 then
  opts.names = names
end
if lsp_mode ~= '' then
  opts.lsp = lsp_mode
end
local job = formatters.format(opts)
local steps = {}
for _, step in ipairs(job.steps or {}) do
  if type(step) == 'string' then
    steps[#steps + 1] = step
  elseif type(step) == 'table' and step.name then
    steps[#steps + 1] = step.name
  end
end
table.sort(steps)
local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
local text = table.concat(lines, '\n')
if #lines > 0 and vim.bo[bufnr].endofline then
  text = text .. '\n'
end
local truncated = false
if #text > 1000000 then
  local cut = 1000000
  while cut > 0 do
    local byte = text:byte(cut)
    if byte < 0x80 or byte >= 0xC0 then
      break
    end
    cut = cut - 1
  end
  text = text:sub(1, cut)
  truncated = true
end
return {
  changed = job.changed == true,
  error = type(job.error) == 'string' and job.error or nil,
  file = path,
  filetype = filetype,
  formatters_applied = steps,
  status = job.status or 'error',
  text = text,
  truncated = truncated,
  written = false,
}
"
    )
}

/// Lint one file with the config's own linter runner
/// (`require('linters')`, the same runner its autocmd pipeline
/// uses). Every selected linter runs through `M.run_linter` and
/// reports through its `on_complete` contract: status, exit code,
/// and the diagnostics it parsed — each diagnostic carrying its
/// producing linter in `source`. Nothing is written to disk.
/// Arguments: path, linter names (empty = the filetype's
/// configured set), per-linter timeout ms, overall budget ms.
pub fn run_lint() -> String {
    format!(
        "{PRELUDE}{body}",
        body = r"
local path, names, per_linter_timeout_ms, budget_ms = ...
local bufnr = ensure_buffer(path)
local filetype = vim.bo[bufnr].filetype
local ok, linters = pcall(require, 'linters')
if not ok then
  return { completed = false, error = 'linters module failed to load: ' .. tostring(linters), file = path, filetype = filetype, results = {} }
end
if #names == 0 then
  local seen, selected = {}, {}
  local function add_all(list)
    for _, name in ipairs(list or {}) do
      if type(name) == 'string' and not seen[name] then
        seen[name] = true
        selected[#selected + 1] = name
      end
    end
  end
  local by_ft = linters.linters_by_ft or {}
  add_all(by_ft[filetype])
  add_all(by_ft['*'])
  names = selected
end
table.sort(names)
if #names == 0 then
  return { completed = true, file = path, filetype = filetype, note = 'no linters configured for this filetype', results = {} }
end
local severity_names = { 'ERROR', 'WARN', 'INFO', 'HINT' }
local results, by_name = {}, {}
local retried = {}
local function collect(result)
  if by_name[result.name] and result.status ~= 'stale' then
    return
  end
  if result.status == 'stale' and not retried[result.name] then
    return
  end
  local diagnostics, truncated = {}, false
  for index, entry in ipairs(result.diagnostics or {}) do
    if index > 500 then
      truncated = true
      break
    end
    diagnostics[#diagnostics + 1] = {
      character = entry.col,
      code = entry.code ~= nil and tostring(entry.code) or nil,
      end_character = entry.end_col,
      end_line = entry.end_lnum,
      line = entry.lnum,
      message = entry.message,
      severity = severity_names[entry.severity] or 'UNKNOWN',
      source = entry.source or result.name,
    }
  end
  local record = {
    diagnostics = diagnostics,
    exit_code = result.exit_code,
    name = result.name,
    reason = result.reason,
    status = result.status or 'error',
    truncated = truncated,
    verified = result.verified == true,
  }
  by_name[result.name] = record
  results[#results + 1] = record
end
local function run_one(name)
  linters.run_linter(name, bufnr, {
    automatic = false,
    notify = false,
    on_complete = collect,
    timeout = per_linter_timeout_ms,
  })
end
for _, name in ipairs(names) do
  run_one(name)
end
local function settled()
  for _, name in ipairs(names) do
    if not by_name[name] then
      return false
    end
  end
  return true
end
local deadline = vim.uv.now() + budget_ms
local function wait_settled()
  local remaining = deadline - vim.uv.now()
  if remaining <= 0 then
    return settled()
  end
  return vim.wait(remaining, settled, 100)
end
wait_settled()
-- A run that reports 'stale' lost a race with the buffer changing
-- under it (the editor's own pipelines converge by running again).
-- Retry each stale linter exactly once inside the same budget.
local stale_names = {}
for _, name in ipairs(names) do
  if not by_name[name] then
    stale_names[#stale_names + 1] = name
  end
end
if #stale_names > 0 and vim.uv.now() < deadline then
  vim.wait(300)
  for _, name in ipairs(stale_names) do
    retried[name] = true
    run_one(name)
  end
  wait_settled()
end
local completed = settled()
table.sort(results, function(a, b) return a.name < b.name end)
local missing = {}
for _, name in ipairs(names) do
  if not by_name[name] then
    missing[#missing + 1] = name
  end
end
return {
  completed = completed and #missing == 0,
  file = path,
  filetype = filetype,
  missing = missing,
  results = results,
}
"
    )
}

/// Every snippet the server can execute, for the syntax gate in tests.
pub fn all_snippets() -> Vec<String> {
    vec![
        STATUS.to_string(),
        SERVERS.to_string(),
        FORMATTERS.to_string(),
        LINTERS.to_string(),
        DAP.to_string(),
        diagnostics(),
        hover(),
        definition(),
        references(),
        document_symbols(),
        run_format(),
        run_lint(),
    ]
}
