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

local M = {}

local UNKNOWN_KEYSPACE  = "SERVER_ERROR unknown keyspace\r\n"
local UDF_NOT_SUPPORTED = "SERVER_ERROR udf not supported\r\n"
local MULTIKEY_UNSUPPORTED = "SERVER_ERROR multi-key not supported\r\n"
local NO_BACKENDS       = "SERVER_ERROR no backends available\r\n"
local MISS_REPLY        = "EN\r\n"

-- Read handler: fan out to M pools, build entries, run merge, return the
-- winning entry's response (or a miss reply).
local function make_read_handler(rctx, handles, pool_names, merge_fn, merge_flags)
    local n_pools = #pool_names
    return function(r)
        local key = r:key()
        if key:find("#", 1, true) then
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
        local winner = merge_fn(entries)
        if winner and winner.res then
            return winner.res
        end
        for _, e in ipairs(entries) do
            if e.status == "miss" and e.res then return e.res end
        end
        for _, e in ipairs(entries) do
            if e.res then return e.res end
        end
        return MISS_REPLY
    end
end

-- Pick the "strongest negative" among write responses.
--   error > not-stored (NS/EX/NF etc) > stored (HD/OK/STORED)
local function reduce_write_all(rctx, handles)
    local worst, worst_rank = nil, -1
    for _, h in ipairs(handles) do
        local res = rctx:res_any(h)
        local rank
        if res == nil or not res:ok() then
            rank = 3  -- error
        elseif res:code() == mcp.MCMC_CODE_STORED
            or res:code() == mcp.MCMC_CODE_DELETED
            or res:code() == mcp.MCMC_CODE_OK then
            rank = 1  -- success
        else
            rank = 2  -- ok protocol but negative (NS/EX/NF)
        end
        if rank > worst_rank then
            worst, worst_rank = res, rank
        end
    end
    if worst then return worst end
    return NO_BACKENDS
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

local function make_write_handler(rctx, handles, policy)
    if policy == "first" then
        return function(r)
            fanout_write(rctx, r, handles)
            return rctx:wait_handle(handles[1])
        end
    end
    -- "all"
    return function(r)
        fanout_write(rctx, r, handles)
        rctx:wait_cond(#handles, mcp.WAIT_ANY)
        return reduce_write_all(rctx, handles)
    end
end

local function read_fgen(ks)
    local fg = mcp.funcgen_new()
    local handles = {}
    for i, pool in ipairs(ks.read_pools) do
        handles[i] = fg:new_handle(pool)
    end
    local pool_names = ks.read_names
    local merge_fn = ks.merge_fn
    local merge_flags = ks.merge_flags or ""
    fg:ready({
        f = function(rctx)
            return make_read_handler(rctx, handles, pool_names, merge_fn, merge_flags)
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
    local policy = ks.write_policy
    fg:ready({
        f = function(rctx)
            return make_write_handler(rctx, handles, policy)
        end,
    })
    return fg
end

local function static_fgen(msg)
    local fg = mcp.funcgen_new()
    fg:ready({
        f = function()
            return function(_r) return msg end
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

    local udf_err = static_fgen(UDF_NOT_SUPPORTED)
    read_map["__udf"]  = udf_err
    write_map["__udf"] = udf_err

    local unknown = static_fgen(UNKNOWN_KEYSPACE)

    local read_router = mcp.router_new({
        map = read_map,
        mode = "prefix",
        stop = ":",
        default = unknown,
    })
    local write_router = mcp.router_new({
        map = write_map,
        mode = "prefix",
        stop = ":",
        default = unknown,
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
