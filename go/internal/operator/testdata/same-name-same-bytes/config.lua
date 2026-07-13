return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
    },
    keyspaces = {
        { prefix = "alpha",
          read = { "mc-a" },
          write = { "mc-a" },
          merge = "shared_merge" },
        { prefix = "beta",
          read = { "mc-a" },
          write = { "mc-a" },
          merge = "shared_merge" },
    },
}
