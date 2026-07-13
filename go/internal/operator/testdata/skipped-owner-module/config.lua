return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
    },
    keyspaces = {
        { prefix = "borrower",
          read = { "mc-a" },
          write = { "mc-a" },
          merge = "donated_merge" },
    },
}
