return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
    },
    keyspaces = {
        { prefix = "borrower",
          read = { "mc-a" },
          write = { "mc-a" },
          merge = "custom_merge" },
        { prefix = "owner",
          read = { "mc-a" },
          write = { "mc-a" },
          merge = "custom_merge" },
    },
}
