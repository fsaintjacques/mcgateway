-- Cross-language contract test: every config.lua the operator's
-- renderer emits must pass this gateway's validator. The golden
-- fixtures under go/internal/operator/testdata/ are the renderer's
-- pinned output; loading each through config.load here means a
-- renderer change that emits something the Lua validator rejects
-- fails `make check` in the same run that produced it.
-- Run: lua tests/test_rendered_config.lua (from the lua/ dir).

package.path = "?.lua;?/init.lua;" .. package.path

-- Permissive fake registry: required_flags never errors. Merge-name
-- resolvability is the renderer's guarantee *relative to registry
-- state* (built-ins plus the snapshot's inline modules, which the
-- committer lands before the config); it cannot be checked statically
-- here. What this test pins is everything else: structure, field
-- names, types, prefix rules, pool references, list shapes.
package.preload["mcgateway_native"] = function()
    return {
        merge = function(_name, _entries) return nil end,
        has_merge = function(_name) return true end,
        required_flags = function(_name) return "" end,
        names = function() return { "first-hit", "last-write-wins", "pool-preferred" } end,
    }
end

local config = require("mcgateway.config")

local TESTDATA = "../go/internal/operator/testdata"

local failed = 0
local function check(cond, name)
    if cond then
        io.stdout:write("ok   " .. name .. "\n")
    else
        io.stderr:write("FAIL " .. name .. "\n")
        failed = failed + 1
    end
end

local function list_cases()
    local out = {}
    local p = assert(io.popen("ls " .. TESTDATA), "cannot list golden cases")
    for line in p:lines() do
        -- Only entries that hold an input.yaml are cases (mirrors the
        -- Go test's IsDir filter); stray files like .DS_Store or
        -- editor backups must not become phantom cases.
        local marker = io.open(TESTDATA .. "/" .. line .. "/input.yaml", "r")
        if marker ~= nil then
            marker:close()
            out[#out + 1] = line
        end
    end
    p:close()
    table.sort(out)
    return out
end

local cases = list_cases()
check(#cases > 0, "golden cases found under " .. TESTDATA)

for _, case in ipairs(cases) do
    local path = TESTDATA .. "/" .. case .. "/config.lua"
    local f = io.open(path, "r")
    if f == nil then
        -- Every renderer output includes config.lua; a case without
        -- one means the goldens and this test have drifted apart.
        check(false, case .. ": golden config.lua exists")
    else
        f:close()
        local ok, err = pcall(config.load, path)
        check(ok, case .. ": rendered config passes the Lua validator"
            .. (ok and "" or (" — " .. tostring(err))))
    end
end

if failed > 0 then os.exit(1) end
