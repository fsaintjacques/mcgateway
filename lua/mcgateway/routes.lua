-- Route dispatch for Stage 1: one funcgen per (keyspace, direction) wrapping
-- a single pool; routers keyed by prefix attach those funcgens to mg/ms/md.
--
-- Memcached's proxy API (1.6.30+) requires funcgens + routers; you can't
-- return a bare handler from mcp.attach once pools are in use.

local M = {}

local UNKNOWN_KEYSPACE = "SERVER_ERROR unknown keyspace\r\n"
local UDF_NOT_SUPPORTED = "SERVER_ERROR udf not supported\r\n"

-- Build a funcgen that dispatches every request to a single pool (passthrough).
local function passthrough_fgen(pool)
    local fg = mcp.funcgen_new()
    local h = fg:new_handle(pool)
    fg:ready({
        f = function(rctx)
            return function(r)
                return rctx:enqueue_and_wait(r, h)
            end
        end,
    })
    return fg
end

-- Funcgen that returns a static string without dispatching to a backend.
local function static_fgen(msg)
    local fg = mcp.funcgen_new()
    fg:ready({
        f = function()
            return function(_r)
                return msg
            end
        end,
    })
    return fg
end

-- Build routers for read (mg) and write (ms/md) sides.
-- keyspaces_built: { prefix = { read_pool, write_pool } }
-- Returns: read_router, write_router.
local function build_routers(keyspaces_built)
    local read_map = {}
    local write_map = {}

    for prefix, ks in pairs(keyspaces_built) do
        read_map[prefix] = passthrough_fgen(ks.read_pool)
        write_map[prefix] = passthrough_fgen(ks.write_pool)
    end

    local udf_err = static_fgen(UDF_NOT_SUPPORTED)
    read_map["__udf"] = udf_err
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
