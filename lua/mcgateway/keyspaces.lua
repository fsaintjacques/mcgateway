local util = require("mcgateway.util")

local M = {}

M._by_prefix = {}

-- Build keyspace lookup table from validated config + a map of pool_name ->
-- pool object returned by mcp.pool().
function M.build(keyspaces_cfg, pools_by_name)
    local out = {}
    for _, ks in ipairs(keyspaces_cfg) do
        out[ks.prefix] = {
            prefix = ks.prefix,
            read_pool = pools_by_name[ks.read],
            write_pool = pools_by_name[ks.write],
        }
    end
    M._by_prefix = out
    return out
end

function M.resolve(key)
    local prefix = util.split_prefix(key)
    if not prefix then return nil end
    return M._by_prefix[prefix]
end

function M.is_udf(key)
    local prefix = util.split_prefix(key)
    return prefix == "__udf"
end

return M
