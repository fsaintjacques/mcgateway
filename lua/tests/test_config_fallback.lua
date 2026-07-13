-- Tests for load_config's last-good fallback: a failing reload must
-- return the previous config (memcached treats an error in
-- mcp_config_pools during SIGHUP reload as fatal), while a failing
-- *first* load stays fatal. Run: lua tests/test_config_fallback.lua
-- (from the lua/ dir).

package.path = "?.lua;?/init.lua;" .. package.path

-- Preload a fake mcgateway_native so config.lua can `require` it
-- without the real cdylib being on cpath. Mirrors the real module's
-- semantics: required_flags *errors* on an unknown name — validation
-- leans on that to reject configs referencing unregistered merges.
local KNOWN_FLAGS = {
    ["first-hit"] = "",
    ["last-write-wins"] = "t",
    ["pool-preferred"] = "",
}
package.preload["mcgateway_native"] = function()
    return {
        merge = function(_name, _entries) return nil end,
        has_merge = function(name) return KNOWN_FLAGS[name] ~= nil end,
        required_flags = function(name)
            local f = KNOWN_FLAGS[name]
            if f == nil then
                error("mcgateway_native: unknown merge " .. tostring(name))
            end
            return f
        end,
        names = function() return { "first-hit", "last-write-wins", "pool-preferred" } end,
    }
end

-- routes.lua is pulled in via the top-level module; stub the mcp global
-- it touches at load time.
mcp = {
    funcgen_new = function() return { new_handle=function() end, ready=function() end } end,
    router_new = function() end,
    attach = function() end,
    request = function() end,
    CMD_MG = 1, CMD_MS = 2, CMD_MD = 3,
    WAIT_ANY = 0, WAIT_GOOD = 1,
    MCMC_CODE_STORED = 8, MCMC_CODE_DELETED = 10, MCMC_CODE_OK = 15,
}

local gw = require("mcgateway")

local failed = 0
local function check(cond, name)
    if cond then
        io.stdout:write("ok   " .. name .. "\n")
    else
        io.stderr:write("FAIL " .. name .. "\n")
        failed = failed + 1
    end
end

local GOOD = [[
return {
    pools = { { name = "mc-a", addrs = { "mc-a:11211" } } },
    keyspaces = { { prefix = "user", read = "mc-a", write = "mc-a" } },
}
]]

local GOOD2 = [[
return {
    pools = { { name = "mc-b", addrs = { "mc-b:11211" } } },
    keyspaces = {
        { prefix = "session", read = "mc-b", write = "mc-b",
          merge = "last-write-wins" },
    },
}
]]

local UNKNOWN_MERGE = [[
return {
    pools = { { name = "mc-a", addrs = { "mc-a:11211" } } },
    keyspaces = {
        { prefix = "user", read = "mc-a", write = "mc-a", merge = "nope" },
    },
}
]]

local BAD_SYNTAX = "retur { this is not lua\n"
local BAD_SEMANTIC = "return { pools = 42 }\n"

local path = os.tmpname()
local function write(content)
    local f = assert(io.open(path, "w"))
    f:write(content)
    f:close()
end

-- First load of a bad config is fatal: a gateway must not start blind.
write(BAD_SYNTAX)
check(not pcall(gw.load_config, path), "first load of bad config errors")

-- Good load caches. Validation resolves required_flags onto the
-- snapshot so route build never queries the registry — the fallback
-- config must stay applicable even after its module leaves the
-- registry (see the step-1 review's High finding).
write(GOOD)
local cfg1 = gw.load_config(path)
check(cfg1.pools[1].name == "mc-a", "good load returns config")
check(cfg1.keyspaces[1].required_flags == "",
    "default merge's required_flags resolved onto snapshot")

-- Failing reloads (syntax, validation, unknown merge) fall back to
-- the last good.
write(BAD_SYNTAX)
check(gw.load_config(path) == cfg1, "syntax-error reload keeps last good")
write(BAD_SEMANTIC)
check(gw.load_config(path) == cfg1, "validation-error reload keeps last good")
write(UNKNOWN_MERGE)
check(gw.load_config(path) == cfg1, "unknown-merge reload keeps last good")

-- A subsequent good reload replaces the cache.
write(GOOD2)
local cfg2 = gw.load_config(path)
check(cfg2.pools[1].name == "mc-b", "good reload returns new config")
check(cfg2.keyspaces[1].required_flags == "t",
    "named merge's required_flags resolved onto snapshot")
write(BAD_SYNTAX)
check(gw.load_config(path) == cfg2, "fallback tracks the newest good config")

os.remove(path)
if failed > 0 then os.exit(1) end
