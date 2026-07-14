-- Route dispatch: per-keyspace funcgens fan out to N read pools and N
-- write pools. Reads run a configured merge function over the per-pool
-- responses; writes honour write_policy (all | first).
--
-- Memcached's proxy API (1.6.30+) requires funcgens + routers; see
-- memory/memcached_proxy_api.md for the canonical pattern.
--
-- `rctx:enqueue(r, handles_table)` broadcasts the header but only sends the
-- request body to the first handle. Fan-out writes therefore build a fresh
-- mcp.request per handle from r:raw_line() and r:raw_value(); fan-out reads
-- (no body) can reuse r directly.

local entries_mod = require("mcgateway.entries")
local mcgw_native = require("mcgateway_native")

local M = {}

local UNKNOWN_KEYSPACE  = "SERVER_ERROR unknown keyspace\r\n"
local UDF_NOT_SUPPORTED = "SERVER_ERROR udf not supported\r\n"
local MULTIKEY_UNSUPPORTED = "SERVER_ERROR multi-key not supported\r\n"
local NO_BACKENDS       = "SERVER_ERROR no backends available\r\n"
local ALL_READ_ERR      = "SERVER_ERROR all backends failed\r\n"
local MISS_REPLY        = "EN\r\n"

-- Read handler: fan out to M pools, build entries, run merge, return the
-- winning entry's response (or a miss reply).
--
-- Instrumentation: `start` is read once at entry and shipped to the
-- native side through the merge call's opts table — the read path's
-- metrics (outcome, per-pool status/latency, merge and request
-- duration) all ride the FFI crossing the dispatch already makes, so
-- the only added call is now().
local function make_read_handler(rctx, handles, prefix, pool_names, merge_name, merge_flags)
    local n_pools = #pool_names
    return function(r)
        local start = mcgw_native.now()
        local key = r:key()
        if key:find("#", 1, true) then
            mcgw_native.observe(prefix, "read", "error", start)
            return MULTIKEY_UNSUPPORTED
        end

        if merge_flags ~= "" then
            for f in merge_flags:gmatch(".") do r:flag_add(f) end
        end
        rctx:enqueue(r, handles)
        rctx:wait_cond(n_pools, mcp.WAIT_ANY)

        local row = {}
        for j = 1, n_pools do
            row[j] = rctx:res_any(handles[j])
        end

        local entries = entries_mod.build(key, pool_names, row)
        local winner_idx = mcgw_native.merge(merge_name, entries,
            { prefix = prefix, start = start })
        if type(winner_idx) == "number" then
            local e = entries[winner_idx]
            if e and e.res then return e.res end
        elseif type(winner_idx) == "string" then
            -- MergeResult::Synthesized: the merge produced fresh bytes
            -- (e.g. the prost-based profile UDF re-encoding a merged
            -- protobuf). Wrap them in the meta `VA` framing the client
            -- is expecting for `mg ... v`.
            --
            -- TODO: if a merge declares `required_flags` *and* returns
            -- Synthesized, any flags the client asked for (t/c/s/…)
            -- need to be echoed here or the client will see a header
            -- it wasn't expecting. Today the two merges that take this
            -- path (profile + concat examples) don't declare flags, so
            -- this is a known gap rather than a bug. When the first
            -- flags-declaring synthesizing UDF ships, extend this to
            -- either reconstruct flags from the entries' lines or
            -- require the merge to return them alongside the bytes.
            return "VA " .. #winner_idx .. "\r\n" .. winner_idx .. "\r\n"
        end

        -- No winner. Distinguish all-miss (a real cache miss) from
        -- all-error (every backend failed). Prefer a concrete miss
        -- response from any backend; fall back to a synthesised `EN` if
        -- at least one backend explicitly missed. Only when every entry
        -- is an error do we surface an error — returning a miss there
        -- would mask backend outages as a not-found.
        local any_miss, concrete_error = false, nil
        for _, e in ipairs(entries) do
            if e.status == "miss" then
                any_miss = true
                if e.res then return e.res end
            elseif e.status == "error" and e.res and concrete_error == nil then
                concrete_error = e.res
            end
        end
        if any_miss then return MISS_REPLY end
        if concrete_error then return concrete_error end
        return ALL_READ_ERR
    end
end

-- Rank a single write response.
--   error (3) > not-stored (NS/EX/NF etc, 2) > stored (HD/OK/STORED, 1)
local function write_rank(res)
    if res == nil or not res:ok() then
        return 3  -- error
    elseif res:code() == mcp.MCMC_CODE_STORED
        or res:code() == mcp.MCMC_CODE_DELETED
        or res:code() == mcp.MCMC_CODE_OK then
        return 1  -- success
    end
    return 2  -- ok protocol but negative (NS/EX/NF)
end

-- Metric outcome label per rank.
local WRITE_OUTCOME = { "stored", "negative", "error" }

-- Pick the "strongest negative" among write responses. Returns the
-- response (or a synthesized error string) plus its rank, so the
-- caller can label metrics without re-deriving the classification.
--
-- Ties within a rank prefer a concrete (non-nil) response: a transport
-- failure on one pool should not suppress another pool's concrete
-- SERVER_ERROR/NS/EX. Only when every pool had a nil response do we
-- fall back to the generic "no backends available".
local function reduce_write_all(rctx, handles)
    local worst, worst_rank = nil, -1
    for _, h in ipairs(handles) do
        local res = rctx:res_any(h)
        local rank = write_rank(res)
        if rank > worst_rank then
            worst, worst_rank = res, rank
        elseif rank == worst_rank and worst == nil and res ~= nil then
            worst = res
        end
    end
    if worst then return worst, worst_rank end
    return NO_BACKENDS, 3
end

-- Build one fresh request per write handle. mcp.request(line, value)
-- produces an independent object; broadcasting the original via
-- rctx:enqueue(r, handles_table) only sends the body to the first pool.
local function fanout_write(rctx, r, handles)
    local line = r:raw_line() .. "\r\n"
    local value = r:raw_value()
    for _, h in ipairs(handles) do
        local sub = mcp.request(line, value)
        rctx:enqueue(sub, h)
    end
end

local function make_write_handler(rctx, handles, prefix, policy)
    if policy == "first" then
        return function(r)
            local start = mcgw_native.now()
            fanout_write(rctx, r, handles)
            local res = rctx:wait_handle(handles[1])
            mcgw_native.observe(prefix, "write",
                WRITE_OUTCOME[write_rank(res)], start)
            return res
        end
    end
    -- "all"
    return function(r)
        local start = mcgw_native.now()
        fanout_write(rctx, r, handles)
        rctx:wait_cond(#handles, mcp.WAIT_ANY)
        local res, rank = reduce_write_all(rctx, handles)
        mcgw_native.observe(prefix, "write", WRITE_OUTCOME[rank], start)
        return res
    end
end

local function read_fgen(ks)
    local fg = mcp.funcgen_new()
    local handles = {}
    for i, pool in ipairs(ks.read_pools) do
        handles[i] = fg:new_handle(pool)
    end
    local prefix = ks.prefix
    local pool_names = ks.read_names
    local merge_name = ks.merge_name
    local merge_flags = ks.merge_flags or ""
    fg:ready({
        f = function(rctx)
            return make_read_handler(rctx, handles, prefix, pool_names,
                merge_name, merge_flags)
        end,
    })
    return fg
end

local function write_fgen(ks)
    local fg = mcp.funcgen_new()
    local handles = {}
    for i, pool in ipairs(ks.write_pools) do
        handles[i] = fg:new_handle(pool)
    end
    local prefix = ks.prefix
    local policy = ks.write_policy
    fg:ready({
        f = function(rctx)
            return make_write_handler(rctx, handles, prefix, policy)
        end,
    })
    return fg
end

-- Constant-reply route that still counts. `prefix` must be one of the
-- fixed sentinel/diagnostic names (__unknown__, __udf, __mcgw), never
-- request-derived: unknown prefixes are unbounded and client-
-- controlled, so they must not mint metric label values.
local function counting_fgen(prefix, op, outcome, msg)
    local fg = mcp.funcgen_new()
    fg:ready({
        f = function()
            return function(_r)
                mcgw_native.observe(prefix, op, outcome, nil)
                return msg
            end
        end,
    })
    return fg
end

local function build_routers(keyspaces_built)
    local read_map, write_map = {}, {}

    for prefix, ks in pairs(keyspaces_built) do
        read_map[prefix]  = read_fgen(ks)
        write_map[prefix] = write_fgen(ks)
    end

    read_map["__udf"]  = counting_fgen("__udf", "read", "error", UDF_NOT_SUPPORTED)
    write_map["__udf"] = counting_fgen("__udf", "write", "error", UDF_NOT_SUPPORTED)

    -- __mcgw: diagnostic prefix. Reads return a single known key:
    --   `mg __mcgw:names v` -> VA <len>\r\n<sorted merge names, comma-joined>\r\n
    -- Used by the kind suite to confirm libmcgateway actually loaded.
    local names_csv = table.concat(mcgw_native.names(), ",")
    local names_reply = string.format("VA %d\r\n%s\r\n", #names_csv, names_csv)
    read_map["__mcgw"]  = counting_fgen("__mcgw", "read", "hit", names_reply)
    write_map["__mcgw"] = counting_fgen("__mcgw", "write", "error", UDF_NOT_SUPPORTED)

    local read_router = mcp.router_new({
        map = read_map,
        mode = "prefix",
        stop = ":",
        default = counting_fgen("__unknown__", "read", "error", UNKNOWN_KEYSPACE),
    })
    local write_router = mcp.router_new({
        map = write_map,
        mode = "prefix",
        stop = ":",
        default = counting_fgen("__unknown__", "write", "error", UNKNOWN_KEYSPACE),
    })
    return read_router, write_router
end

function M.attach(keyspaces_built)
    local read_router, write_router = build_routers(keyspaces_built)
    mcp.attach(mcp.CMD_MG, read_router)
    mcp.attach(mcp.CMD_MS, write_router)
    mcp.attach(mcp.CMD_MD, write_router)
end

return M
