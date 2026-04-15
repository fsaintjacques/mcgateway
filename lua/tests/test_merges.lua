-- Unit tests for mcgateway.merges.
-- Run: lua tests/test_merges.lua (from the lua/ dir).

package.path = "?.lua;?/init.lua;" .. package.path

local merges = require("mcgateway.merges")

local failed = 0
local function check(cond, name)
    if cond then
        io.stdout:write("ok   " .. name .. "\n")
    else
        io.stderr:write("FAIL " .. name .. "\n")
        failed = failed + 1
    end
end

local function hit(key, pool, t) return { key=key, pool=pool, status="hit", t=t, res={} } end
local function miss(key, pool)  return { key=key, pool=pool, status="miss", res={} } end
local function errE(key, pool)  return { key=key, pool=pool, status="error", res=nil } end

-- first-hit ---------------------------------------------------------------
do
    local e1, e2 = hit("k", "a"), hit("k", "b")
    check(merges.first_hit({ e1, e2 }) == e1, "first_hit returns first entry in order")
end
do
    local e1, e2 = miss("k","a"), hit("k","b")
    check(merges.first_hit({ e1, e2 }) == e2, "first_hit skips misses")
end
do
    local e1, e2 = errE("k","a"), hit("k","b")
    check(merges.first_hit({ e1, e2 }) == e2, "first_hit skips errors")
end
do
    check(merges.first_hit({ miss("k","a"), miss("k","b") }) == nil,
          "first_hit all miss -> nil")
end
do
    check(merges.first_hit({ errE("k","a"), errE("k","b") }) == nil,
          "first_hit all error -> nil")
end
do
    check(merges.first_hit({}) == nil, "first_hit empty -> nil")
end

-- pool-preferred: same as first-hit -------------------------------------
check(merges.pool_preferred == merges.first_hit, "pool_preferred aliases first_hit")

-- last-write-wins ---------------------------------------------------------
do
    local e1, e2, e3 = hit("k","a", 100), hit("k","b", 300), hit("k","c", 200)
    check(merges.last_write_wins({ e1, e2, e3 }) == e2, "lww picks highest t")
end
do
    -- Ties: first wins (strict > keeps the earlier one).
    local e1, e2 = hit("k","a", 50), hit("k","b", 50)
    check(merges.last_write_wins({ e1, e2 }) == e1, "lww tie keeps first")
end
do
    -- Missing t is treated as "unknown, older than any known t": a later
    -- entry with a concrete t replaces a best whose t is nil.
    local e1, e2 = hit("k","a", nil), hit("k","b", 500)
    check(merges.last_write_wins({ e1, e2 }) == e2,
          "lww entry with t replaces nil-t anchor")
end
do
    -- Conversely, an entry with known t is not displaced by a later entry
    -- with nil t.
    local e1, e2 = hit("k","a", 100), hit("k","b", nil)
    check(merges.last_write_wins({ e1, e2 }) == e1, "lww ignores later nil-t entry")
end
do
    check(merges.last_write_wins({ miss("k","a"), errE("k","b") }) == nil,
          "lww no hits -> nil")
end

-- lookup / names ----------------------------------------------------------
check(merges.lookup("first-hit") == merges.first_hit, "lookup first-hit")
check(merges.lookup("pool-preferred") == merges.pool_preferred, "lookup pool-preferred")
check(merges.lookup("last-write-wins") == merges.last_write_wins, "lookup lww")
check(merges.lookup("bogus") == nil, "lookup unknown -> nil")

do
    local ns = merges.names()
    table.sort(ns)
    local want = { "first-hit", "last-write-wins", "pool-preferred" }
    local ok = #ns == #want
    for i = 1, #want do ok = ok and ns[i] == want[i] end
    check(ok, "names() returns all three sorted")
end

if failed > 0 then
    io.stderr:write(string.format("%d test(s) failed\n", failed))
    os.exit(1)
end
