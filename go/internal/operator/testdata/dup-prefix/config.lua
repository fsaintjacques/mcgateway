return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
    },
    keyspaces = {
        { prefix = "contested",
          read = { "mc-a" },
          write = { "mc-a" },
          write_policy = "first" },
    },
}
