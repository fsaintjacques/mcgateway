return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
        { name = "mc-b", addrs = { "mc-b:11211" } },
        { name = "mc-c", addrs = { "mc-c:11211" } },
    },

    keyspaces = {
        -- Single-pool passthrough (Stage 1 shape still works).
        {
            prefix = "user",
            read   = "mc-a",
            write  = "mc-a",
        },
        -- Fan-out reads with pool-preferred merge (read from mc-a first,
        -- fall back to mc-b). Writes go to both (policy=all).
        {
            prefix       = "session",
            read         = { "mc-a", "mc-b" },
            write        = { "mc-a", "mc-b" },
            write_policy = "all",
            merge        = "pool-preferred",
        },
        -- Migration scenario: read from old + new, pick freshest; writes
        -- go to the new primary synchronously, shadow to the old.
        {
            prefix       = "cache",
            read         = { "mc-b", "mc-c" },
            write        = { "mc-c", "mc-b" },
            write_policy = "first",
            merge        = "last-write-wins",
        },
    },
}
