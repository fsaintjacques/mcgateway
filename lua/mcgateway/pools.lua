local M = {}

local function parse_addr(addr)
    local host, port = addr:match("^(.+):(%d+)$")
    if not host then
        error("mcgateway: invalid addr " .. tostring(addr) .. " (expected host:port)", 0)
    end
    return host, tonumber(port)
end

local function make_backend(pool_name, addr)
    local host, port = parse_addr(addr)
    local label = pool_name .. "/" .. addr
    return mcp.backend(label, host, port)
end

function M.build(pools_cfg)
    local out = {}
    for _, p in ipairs(pools_cfg) do
        local backends = {}
        for i, addr in ipairs(p.addrs) do
            backends[i] = make_backend(p.name, addr)
        end
        local opts = {}
        if p.hash then opts.hash = p.hash end
        if p.dist then opts.dist = p.dist end
        out[p.name] = mcp.pool(backends, opts)
    end
    return out
end

return M
