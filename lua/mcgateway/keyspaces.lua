local M = {}

-- Build keyspace lookup table from validated config + a map of pool_name ->
-- pool object returned by mcp.pool(). Each keyspace ends up with a list of
-- pool objects (labelled by the config's pool-name list for entry
-- attribution), a write policy, and a resolved merge function.
function M.build(keyspaces_cfg, pools_by_name)
    local out = {}
    for _, ks in ipairs(keyspaces_cfg) do
        local read_pools = {}
        for i, name in ipairs(ks.read) do
            read_pools[i] = pools_by_name[name]
        end
        local write_pools = {}
        for i, name in ipairs(ks.write) do
            write_pools[i] = pools_by_name[name]
        end
        out[ks.prefix] = {
            prefix = ks.prefix,
            read_pools = read_pools,
            read_names = ks.read,
            write_pools = write_pools,
            write_policy = ks.write_policy,
            merge_name = ks.merge,
            -- Resolved at validation time and carried on the config
            -- snapshot. Never query the registry here: this runs in
            -- every worker VM on reload, including reloads that fell
            -- back to the last good config after its module vanished
            -- from the UDF directory — a registry miss here would be
            -- an error inside the reload lifecycle, which is fatal.
            merge_flags = ks.required_flags,
        }
    end
    return out
end

return M
