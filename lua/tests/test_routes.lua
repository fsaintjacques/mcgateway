-- Smoke-load mcgateway.routes under a stubbed mcp global to catch syntax or
-- reference errors. Behavior is covered by the kind integration suite.
-- Run: lua tests/test_routes.lua (from the lua/ dir).

package.path = "?.lua;?/init.lua;" .. package.path

mcp = {
    funcgen_new = function() return { new_handle=function() end, ready=function() end } end,
    router_new = function() end,
    attach = function() end,
    request = function() end,
    CMD_MG = 1, CMD_MS = 2, CMD_MD = 3,
    WAIT_ANY = 0, WAIT_GOOD = 1,
    MCMC_CODE_STORED = 8, MCMC_CODE_DELETED = 10, MCMC_CODE_OK = 15,
}

local routes = require("mcgateway.routes")
assert(type(routes.attach) == "function", "routes.attach missing")
io.stdout:write("ok   routes module loads\n")
