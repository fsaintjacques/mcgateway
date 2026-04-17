local M = {}

function M.log(fmt, ...)
    io.stderr:write("[mcgateway] " .. string.format(fmt, ...) .. "\n")
end

return M
