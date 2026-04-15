local M = {}

function M.split_prefix(key)
    if type(key) ~= "string" then return nil, nil end
    local colon = key:find(":", 1, true)
    if not colon or colon == 1 then return nil, nil end
    return key:sub(1, colon - 1), key:sub(colon + 1)
end

function M.log(fmt, ...)
    io.stderr:write("[mcgateway] " .. string.format(fmt, ...) .. "\n")
end

return M
