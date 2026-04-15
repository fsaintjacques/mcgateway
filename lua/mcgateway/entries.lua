-- Build entry tables from per-pool responses.
--
-- Entry shape:
--   { key, pool, status = "hit"|"miss"|"error", res, t }
--
-- `res` is the mcp.response userdata or nil. `t` is the parsed integer
-- value of the meta `t` flag (TTL remaining) when present in the response
-- line, otherwise nil. Merge functions that want other flags can parse
-- `res:line()` themselves.

local M = {}

-- Status classification.
--   hit:    res:ok() and res:hit()
--   miss:   res:ok() and not res:hit() (e.g. EN, NF)
--   error:  res == nil or not res:ok() (connection/proto/timeout)
local function classify(res)
    if res == nil then return "error" end
    if not res:ok() then return "error" end
    if res:hit() then return "hit" end
    return "miss"
end

-- Parse the `t` flag out of a meta response line. The line for a meta get
-- hit looks like: "VA <len> t<int> s<int> c<int> ...".
local function parse_t(line)
    if line == nil then return nil end
    for tok in line:gmatch("%S+") do
        local v = tok:match("^t(%-?%d+)$")
        if v then return tonumber(v) end
    end
    return nil
end

-- Build an entry from a single (key, pool, res) triple.
--
-- For hits, `value` is materialised eagerly: WASM merges may inspect
-- the response body (e.g. to decode protobuf), and the Rust side has
-- no way to reach back into the Lua userdata once projection ends.
-- Built-in merges don't touch `value`/`line`, so the allocation here
-- is only paid on hits (one string per pool) — memcached's Lua API
-- allocates a fresh string on each `res:value()` call regardless.
function M.make(key, pool_name, res)
    local status = classify(res)
    local t, value, line
    if status == "hit" and res then
        line = res:line()
        if line then t = parse_t(line) end
        -- memcached's mcp.response has no direct value accessor. The
        -- raw buffer is `<line>\r\n<value bytes>\r\n`; res:line()
        -- returns just the *tail* of the header (everything after
        -- the response code, so "VA 10 t-1\r\n" appears as "10 t-1")
        -- which makes a line-length-based offset unreliable. Use the
        -- known vlen() instead: the value is the last vlen bytes
        -- before the trailing CRLF.
        local vlen = res:vlen()
        if vlen and vlen > 0 then
            local raw = res:raw_string()
            if raw and #raw >= vlen + 2 then
                value = raw:sub(#raw - vlen - 1, #raw - 2)
            end
        end
    end
    return {
        key = key,
        pool = pool_name,
        status = status,
        res = res,
        t = t,
        value = value,
        line = line,
    }
end

-- Build the ordered entry list for a single-key fan-out.
--   key:        the request key
--   pool_names: { "frostmap", "mc-cluster", ... } in the keyspace's read order
--   results:    results[j] is the mcp.response for pool_names[j] (nil on error)
--
-- Entries come back in pool-index order — the merge contract.
function M.build(key, pool_names, results)
    local out = {}
    for j, p in ipairs(pool_names) do
        out[j] = M.make(key, p, results[j])
    end
    return out
end

-- Exposed for unit tests.
M._classify = classify
M._parse_t = parse_t

return M
