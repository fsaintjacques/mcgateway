local config = require("mcgateway.config")
local pools = require("mcgateway.pools")
local keyspaces = require("mcgateway.keyspaces")
local routes = require("mcgateway.routes")
local util = require("mcgateway.util")

local M = {
    _config = nil,
    _config_path = nil,
}

function M.load_config(path)
    M._config_path = path
    M._config = config.load(path)
    util.log("loaded config from %s: %d pools, %d keyspaces",
        path, #M._config.pools, #M._config.keyspaces)
    return M._config
end

function M.reload()
    if not M._config_path then
        error("mcgateway: reload called before load_config", 0)
    end
    return M.load_config(M._config_path)
end

-- use_config installs an already-validated config table into module state
-- without reading from disk. Lets mcp_config_routes pick up the exact
-- snapshot mcp_config_pools loaded, so a concurrent edit to the config
-- file between those phases cannot desync pools from routes.
function M.use_config(cfg)
    M._config = cfg
end

-- build_pools() is intended to be called from mcp_config_pools(). It returns
-- a { pool_name = pool_obj } table that memcached passes to the per-worker
-- mcp_config_routes hook, where it must be handed to build_routes().
function M.build_pools()
    assert(M._config, "mcgateway: load_config must run before build_pools")
    return pools.build(M._config.pools)
end

-- build_routes(pools_by_name) is intended to be called from mcp_config_routes.
-- Accepts the table returned by build_pools().
function M.build_routes(pools_by_name)
    assert(M._config, "mcgateway: load_config must run before build_routes")
    local built = keyspaces.build(M._config.keyspaces, pools_by_name)
    routes.attach(built)
end

M.config = config
M.pools = pools
M.keyspaces = keyspaces
M.routes = routes
M.util = util

return M
