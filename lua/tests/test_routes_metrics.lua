-- Handler-level tests for the instrumentation in routes.lua: the read
-- path must ship prefix/start through the merge call's opts table, the
-- write path must classify outcomes onto observe(), and the sentinel
-- routes must count under their fixed names. Uses a fake mcp with
-- capturing funcgens and a scripted rctx; the real proxy plumbing is
-- the kind suite's job.
-- Run: lua tests/test_routes_metrics.lua (from the lua/ dir).

package.path = "?.lua;?/init.lua;" .. package.path

-- Recording fake native. now() ticks so start values are distinct and
-- observable; merge records its opts and returns a scripted value.
local native = {
    clock = 1000,
    merge_calls = {},
    observe_calls = {},
    merge_returns = 1,
}
package.preload["mcgateway_native"] = function()
    return {
        merge = function(name, entries, opts)
            native.merge_calls[#native.merge_calls + 1] =
                { name = name, entries = entries, opts = opts }
            return native.merge_returns
        end,
        has_merge = function(_name) return true end,
        required_flags = function(_name) return "" end,
        names = function() return { "first-hit" } end,
        now = function()
            native.clock = native.clock + 1
            return native.clock
        end,
        observe = function(prefix, op, outcome, start)
            native.observe_calls[#native.observe_calls + 1] =
                { prefix = prefix, op = op, outcome = outcome, start = start }
        end,
        observe_reload = function() end,
    }
end

-- Fake mcp: funcgens capture their ready-function, routers capture
-- their map and default so tests can reach every handler.
local routers = {}
mcp = {
    funcgen_new = function()
        local fg = { handles = {} }
        fg.new_handle = function(_, pool)
            fg.handles[#fg.handles + 1] = pool
            return #fg.handles
        end
        fg.ready = function(_, opts) fg.f = opts.f end
        return fg
    end,
    router_new = function(opts)
        routers[#routers + 1] = opts
        return opts
    end,
    attach = function() end,
    request = function(line, value)
        return { _line = line, _value = value }
    end,
    CMD_MG = 1, CMD_MS = 2, CMD_MD = 3,
    WAIT_ANY = 0, WAIT_GOOD = 1,
    MCMC_CODE_STORED = 8, MCMC_CODE_DELETED = 10, MCMC_CODE_OK = 15,
}

local routes = require("mcgateway.routes")

local failed = 0
local function check(cond, name)
    if cond then
        io.stdout:write("ok   " .. name .. "\n")
    else
        io.stderr:write("FAIL " .. name .. "\n")
        failed = failed + 1
    end
end

-- Response fakes ------------------------------------------------------------
local function hit_res(code)
    return {
        ok = function() return true end,
        hit = function() return true end,
        code = function() return code or mcp.MCMC_CODE_STORED end,
        line = function() return "3 t-1" end,
        vlen = function() return 0 end,
        raw_string = function() return nil end,
        elapsed = function() return 500 end,
    }
end
local function negative_res()
    local r = hit_res()
    r.hit = function() return false end
    r.code = function() return 99 end -- NS-ish: ok protocol, not stored
    return r
end

-- A scripted rctx: res_any/wait_handle serve from a per-handle table.
local function fake_rctx(responses)
    return {
        enqueued = {},
        enqueue = function(self, r, h) self.enqueued[#self.enqueued + 1] = { r, h } end,
        wait_cond = function() end,
        res_any = function(_, h) return responses[h] end,
        wait_handle = function(_, h) return responses[h] end,
    }
end

local fake_request = {
    key = function() return "user:1" end,
    flag_add = function() end,
    raw_line = function() return "ms user:1 3" end,
    raw_value = function() return "abc" end,
}

-- Build routes for one two-pool keyspace ------------------------------------
routes.attach({
    user = {
        prefix = "user",
        read_pools = { "pa", "pb" },
        read_names = { "a", "b" },
        write_pools = { "pa", "pb" },
        write_policy = "all",
        merge_name = "first-hit",
        merge_flags = "",
    },
})
local read_map = routers[1].map
local write_map = routers[2].map

-- Read path ------------------------------------------------------------------
do
    local handler = read_map["user"].f(fake_rctx({ hit_res(), hit_res() }))
    local res = handler(fake_request)
    local call = native.merge_calls[#native.merge_calls]
    check(call.name == "first-hit", "read: merge dispatched")
    check(call.opts and call.opts.prefix == "user",
        "read: keyspace prefix rides the merge opts")
    check(type(call.opts.start) == "number" and call.opts.start > 1000,
        "read: start timestamp rides the merge opts")
    check(call.entries[1].elapsed == 500,
        "read: entries carry backend elapsed for the native side")
    check(res ~= nil, "read: winner's response returned")
end

-- Multi-key rejection counts as a request error on the keyspace ---------------
do
    local handler = read_map["user"].f(fake_rctx({}))
    local res = handler({ key = function() return "user:1#user:2" end })
    check(res:find("SERVER_ERROR", 1, true) == 1, "multikey: error reply")
    local ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "user" and ob.op == "read" and ob.outcome == "error"
        and type(ob.start) == "number",
        "multikey: observed as read error with duration")
end

-- Write path: policy all -------------------------------------------------------
do
    local handler = write_map["user"].f(fake_rctx({ hit_res(), hit_res() }))
    handler(fake_request)
    local ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "user" and ob.op == "write" and ob.outcome == "stored",
        "write all: both stored observed as stored")

    handler = write_map["user"].f(fake_rctx({ hit_res(), negative_res() }))
    handler(fake_request)
    ob = native.observe_calls[#native.observe_calls]
    check(ob.outcome == "negative", "write all: one NS observed as negative")

    handler = write_map["user"].f(fake_rctx({ hit_res(), nil }))
    handler(fake_request)
    ob = native.observe_calls[#native.observe_calls]
    check(ob.outcome == "error", "write all: one transport failure observed as error")
end

-- Write path: policy first ------------------------------------------------------
do
    routes.attach({
        wfirst = {
            prefix = "wfirst",
            read_pools = { "pa" },
            read_names = { "a" },
            write_pools = { "pa", "pb" },
            write_policy = "first",
            merge_name = "first-hit",
            merge_flags = "",
        },
    })
    local wmap = routers[#routers].map
    local handler = wmap["wfirst"].f(fake_rctx({ negative_res(), hit_res() }))
    handler(fake_request)
    local ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "wfirst" and ob.op == "write" and ob.outcome == "negative",
        "write first: primary's rank classified, secondary ignored")
end

-- Sentinel routes ---------------------------------------------------------------
do
    local udf = read_map["__udf"].f(fake_rctx({}))
    udf(fake_request)
    local ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "__udf" and ob.op == "read" and ob.outcome == "error"
        and ob.start == nil,
        "__udf read counted as error, no duration")

    local names = read_map["__mcgw"].f(fake_rctx({}))
    local reply = names(fake_request)
    ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "__mcgw" and ob.outcome == "hit",
        "__mcgw names read counted as hit")
    check(reply:find("first%-hit") ~= nil, "__mcgw reply carries merge names")

    local unknown = routers[1].default.f(fake_rctx({}))
    unknown(fake_request)
    ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "__unknown__" and ob.op == "read" and ob.outcome == "error",
        "unknown keyspace counted under the fixed sentinel")

    local wunknown = routers[2].default.f(fake_rctx({}))
    wunknown(fake_request)
    ob = native.observe_calls[#native.observe_calls]
    check(ob.prefix == "__unknown__" and ob.op == "write",
        "unknown write counted under the sentinel with op=write")
end

if failed > 0 then
    io.stderr:write(string.format("%d test(s) failed\n", failed))
    os.exit(1)
end
