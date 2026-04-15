-- Built-in merge functions operating on an ordered entry list.
--
-- Entry shape (see entries.lua):
--   { key, pool, status = "hit"|"miss"|"error", res, t }
--
-- A merge returns the winning entry (nil for "no winner" / miss).
-- The caller forwards `entry.res` as the response.
--
-- Ordering contract: entries are grouped by key in request order, and within
-- each key by the pool's index in the keyspace's read list.

local M = {}

function M.first_hit(entries)
    for _, e in ipairs(entries) do
        if e.status == "hit" then return e end
    end
    return nil
end

-- Identical to first_hit given the ordering contract; kept distinct for
-- intent at the config call site.
M.pool_preferred = M.first_hit

function M.last_write_wins(entries)
    local best
    for _, e in ipairs(entries) do
        if e.status == "hit" then
            if best == nil then
                best = e
            elseif e.t ~= nil and (best.t == nil or e.t > best.t) then
                best = e
            end
        end
    end
    return best
end

-- `flags` lists the single-character meta flags the merge needs backends
-- to return. The gateway augments outgoing reads with these flags so the
-- merge sees the values it relies on.
M._by_name = {
    ["first-hit"]       = { fn = M.first_hit,       flags = "" },
    ["pool-preferred"]  = { fn = M.pool_preferred,  flags = "" },
    ["last-write-wins"] = { fn = M.last_write_wins, flags = "t" },
}

function M.lookup(name)
    local entry = M._by_name[name]
    if entry then return entry.fn end
    return nil
end

function M.required_flags(name)
    local entry = M._by_name[name]
    if entry then return entry.flags end
    return ""
end

function M.names()
    local ns = {}
    for n in pairs(M._by_name) do ns[#ns+1] = n end
    table.sort(ns)
    return ns
end

return M
