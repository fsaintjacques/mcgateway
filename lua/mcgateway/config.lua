local M = {}

local VALID_HASH = { xxhash = true, md5 = true, crc32 = true }
local VALID_DIST = { ring_hash = true, jump_hash = true }

local function err(fmt, ...)
    error("mcgateway config: " .. string.format(fmt, ...), 0)
end

local function validate_pool(p, seen)
    if type(p.name) ~= "string" or p.name == "" then
        err("pool missing name")
    end
    if seen[p.name] then
        err("duplicate pool name %q", p.name)
    end
    seen[p.name] = true
    if type(p.addrs) ~= "table" or #p.addrs == 0 then
        err("pool %q: addrs must be a non-empty list", p.name)
    end
    for i, a in ipairs(p.addrs) do
        if type(a) ~= "string" or a == "" then
            err("pool %q: addrs[%d] must be a non-empty string", p.name, i)
        end
    end
    if p.hash ~= nil and not VALID_HASH[p.hash] then
        err("pool %q: invalid hash %q", p.name, tostring(p.hash))
    end
    if p.dist ~= nil and not VALID_DIST[p.dist] then
        err("pool %q: invalid dist %q", p.name, tostring(p.dist))
    end
end

local function validate_keyspace(ks, seen_prefix, pool_names)
    if type(ks.prefix) ~= "string" or ks.prefix == "" then
        err("keyspace missing prefix")
    end
    if ks.prefix:find(":", 1, true) then
        err("keyspace prefix %q must not contain ':'", ks.prefix)
    end
    if ks.prefix == "__udf" then
        err("keyspace prefix %q is reserved", ks.prefix)
    end
    if seen_prefix[ks.prefix] then
        err("duplicate keyspace prefix %q", ks.prefix)
    end
    seen_prefix[ks.prefix] = true
    if type(ks.read) ~= "string" or not pool_names[ks.read] then
        err("keyspace %q: unknown read pool %q", ks.prefix, tostring(ks.read))
    end
    if type(ks.write) ~= "string" or not pool_names[ks.write] then
        err("keyspace %q: unknown write pool %q", ks.prefix, tostring(ks.write))
    end
end

function M.validate(cfg)
    if type(cfg) ~= "table" then err("config must be a table") end
    if type(cfg.pools) ~= "table" then err("config.pools must be a list") end
    if type(cfg.keyspaces) ~= "table" then err("config.keyspaces must be a list") end

    local pool_names = {}
    for _, p in ipairs(cfg.pools) do
        validate_pool(p, pool_names)
    end
    local seen_prefix = {}
    for _, ks in ipairs(cfg.keyspaces) do
        validate_keyspace(ks, seen_prefix, pool_names)
    end
    return cfg
end

function M.load(path)
    local chunk, loaderr = loadfile(path)
    if not chunk then
        err("cannot load %s: %s", path, loaderr)
    end
    local ok, result = pcall(chunk)
    if not ok then
        err("error running %s: %s", path, tostring(result))
    end
    return M.validate(result)
end

return M
