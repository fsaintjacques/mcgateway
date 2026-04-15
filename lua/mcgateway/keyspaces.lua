local util = require("mcgateway.util")
local mcgw_native = require("mcgateway_native")

local M = {}

M._by_prefix = {}

-- Build keyspace lookup table from validated config + a map of pool_name ->
-- pool object returned by mcp.pool(). Each keyspace ends up with lists of
-- pool objects (paired with their names for entry labeling), a write policy,
-- and a resolved merge function.
function M.build(keyspaces_cfg, pools_by_name)
    local out = {}
    for _, ks in ipairs(keyspaces_cfg) do
        local read_pools, read_names = {}, {}
        for i, name in ipairs(ks.read) do
            read_pools[i] = pools_by_name[name]
            read_names[i] = name
        end
        local write_pools, write_names = {}, {}
        for i, name in ipairs(ks.write) do
            write_pools[i] = pools_by_name[name]
            write_names[i] = name
        end
        out[ks.prefix] = {
            prefix = ks.prefix,
            read_pools = read_pools,
            read_names = read_names,
            write_pools = write_pools,
            write_names = write_names,
            write_policy = ks.write_policy,
            merge_name = ks.merge,
            merge_flags = mcgw_native.required_flags(ks.merge),
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
