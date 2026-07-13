return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211", "mc-a-2:11211" }, hash = "xxhash", dist = "ring_hash" },
        { name = "mc-b", addrs = { "mc-b:11211" }, hash = "md5", dist = "jump_hash" },
    },
    keyspaces = {
        { prefix = "profile",
          read = { "mc-b", "mc-a" },
          write = { "mc-b" },
          write_policy = "all",
          merge = "last-write-wins" },
        { prefix = "session",
          read = { "mc-a", "mc-b" },
          write = { "mc-a", "mc-b" },
          write_policy = "first",
          merge = "pool-preferred" },
    },
}
