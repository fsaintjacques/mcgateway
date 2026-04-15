-- Unit tests for mcgateway.entries.
-- Run: lua tests/test_entries.lua (from the lua/ dir).

package.path = "?.lua;?/init.lua;" .. package.path

local entries = require("mcgateway.entries")

local failed = 0
local function check(cond, name)
    if cond then
        io.stdout:write("ok   " .. name .. "\n")
    else
        io.stderr:write("FAIL " .. name .. "\n")
        failed = failed + 1
    end
end

-- parse_t -----------------------------------------------------------------
check(entries._parse_t("VA 5 t3600 s100") == 3600, "parse_t finds t in meta line")
check(entries._parse_t("VA 0 c7 t120")    == 120,  "parse_t finds t at end")
check(entries._parse_t("VA 5 s100 c7")    == nil,  "parse_t nil when absent")
check(entries._parse_t("VA 5 T3600")      == nil,  "parse_t case-sensitive (T != t)")
check(entries._parse_t("VA 5 t-1")        == -1,   "parse_t handles negative")
check(entries._parse_t(nil)               == nil,  "parse_t nil line -> nil")
check(entries._parse_t("EN")              == nil,  "parse_t on miss line -> nil")

-- classify ----------------------------------------------------------------
local function fake_res(ok, hit)
    return { ok = function() return ok end, hit = function() return hit end }
end
check(entries._classify(fake_res(true,  true))  == "hit",   "classify ok+hit")
check(entries._classify(fake_res(true,  false)) == "miss",  "classify ok+!hit -> miss")
check(entries._classify(fake_res(false, false)) == "error", "classify !ok -> error")
check(entries._classify(nil)                    == "error", "classify nil -> error")

-- build: pool-ordered entries for a single key ----------------------------
do
    local function stub_res(tag) return {
        ok=function() return true end, hit=function() return true end,
        line=function() return "VA 3" end,
        vlen=function() return 0 end,
        raw_string=function() return nil end,
        _tag=tag,
    } end
    local pools = { "a", "b" }
    local results = { stub_res("a"), stub_res("b") }
    local es = entries.build("user:1", pools, results)
    check(#es == 2, "build yields one entry per pool")
    check(es[1].key == "user:1" and es[1].pool == "a", "build [1] = (key, a)")
    check(es[2].key == "user:1" and es[2].pool == "b", "build [2] = (key, b)")
end

-- build with nil result cells (per-pool error) ----------------------------
do
    local pools = { "a", "b" }
    local es = entries.build("user:1", pools, { nil, nil })
    check(es[1].status == "error" and es[2].status == "error",
          "nil response cells classified as error")
end

if failed > 0 then
    io.stderr:write(string.format("%d test(s) failed\n", failed))
    os.exit(1)
end
