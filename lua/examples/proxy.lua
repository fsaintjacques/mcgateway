-- Make the mcgateway library importable regardless of where memcached
-- decides to put its default package.path.
local LUA_ROOT = os.getenv("MCGATEWAY_LUA_ROOT") or "/etc/mcgateway/lua"
package.path = LUA_ROOT .. "/?.lua;" .. LUA_ROOT .. "/?/init.lua;" .. package.path
package.cpath = LUA_ROOT .. "/?.so;" .. package.cpath

local gw = require("mcgateway")

local CONFIG_PATH = os.getenv("MCGATEWAY_CONFIG") or "/etc/mcgateway/config.lua"

-- mcp_config_pools runs in a one-off config Lua state; its return value is
-- handed to mcp_config_routes in each worker's Lua state. Load the config
-- once here and ship it alongside the pool table so workers attach routes
-- against the exact same config snapshot that built the pools. Re-reading
-- the file from each worker would race reloads and let routes reference
-- pools that no longer exist (or miss pools that now do).
function mcp_config_pools()
    local cfg = gw.load_config(CONFIG_PATH)
    return { pools = gw.build_pools(), config = cfg }
end

function mcp_config_routes(bundle)
    gw.use_config(bundle.config)
    gw.build_routes(bundle.pools)
end
