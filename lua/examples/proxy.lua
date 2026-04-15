-- Make the mcgateway library importable regardless of where memcached
-- decides to put its default package.path.
local LUA_ROOT = os.getenv("MCGATEWAY_LUA_ROOT") or "/etc/mcgateway/lua"
package.path = LUA_ROOT .. "/?.lua;" .. LUA_ROOT .. "/?/init.lua;" .. package.path
package.cpath = LUA_ROOT .. "/?.so;" .. package.cpath

local gw = require("mcgateway")

gw.load_config(os.getenv("MCGATEWAY_CONFIG") or "/etc/mcgateway/config.lua")

-- mcp_config_pools runs in a one-off config Lua state; its return value is
-- handed to mcp_config_routes in each worker's Lua state.
function mcp_config_pools()
    return gw.build_pools()
end

function mcp_config_routes(pools)
    -- load_config again so the worker's own state has the keyspace table;
    -- re-reading is cheap and keeps per-worker state self-contained.
    gw.load_config(os.getenv("MCGATEWAY_CONFIG") or "/etc/mcgateway/config.lua")
    gw.build_routes(pools)
end
