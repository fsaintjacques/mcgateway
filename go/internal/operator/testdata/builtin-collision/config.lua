return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
    },
    keyspaces = {
        { prefix = "shadowed",
          read = { "mc-a" },
          write = { "mc-a" },
          merge = "first-hit" },
    },
}
