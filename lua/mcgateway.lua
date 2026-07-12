local config = require("mcgateway.config")
local pools = require("mcgateway.pools")
local keyspaces = require("mcgateway.keyspaces")
local routes = require("mcgateway.routes")
local util = require("mcgateway.util")

local M = {
    _config = nil,
    _last_good = nil,
}

-- load_config reads and validates the config file. A failure on the
-- *first* load is fatal: a gateway must not start without a config.
-- A failure on a later load (SIGHUP reload) falls back to the last
-- good config instead of erroring, because memcached treats an error
-- in mcp_config_pools during reload as fatal and exits — a truncated
-- or half-written config file must degrade to "keep serving the old
-- routes", never to an outage. Module state survives reloads (the
-- proxy re-runs mcp_config_pools in the same config VM, where
-- package.loaded caches this table), which is what makes the
-- fallback possible.
function M.load_config(path)
    local ok, cfg = pcall(config.load, path)
    if not ok then
        if M._last_good == nil then
            error(cfg, 0)
        end
        util.log("config reload from %s failed, keeping previous config: %s",
            path, tostring(cfg))
        M._config = M._last_good
        return M._config
    end
    M._last_good = cfg
    M._config = cfg
    util.log("loaded config from %s: %d pools, %d keyspaces",
        path, #cfg.pools, #cfg.keyspaces)
    return cfg
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
