return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
    },
    keyspaces = {
        { prefix = "good",
          read = { "mc-a" },
          write = { "mc-a" } },
    },
}
