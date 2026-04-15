return {
    pools = {
        {
            name = "mc-a",
            addrs = { "mc-a:11211" },
        },
        {
            name = "mc-b",
            addrs = { "mc-b:11211" },
        },
    },

    keyspaces = {
        {
            prefix = "user",
            read   = "mc-a",
            write  = "mc-a",
        },
        {
            prefix = "session",
            read   = "mc-b",
            write  = "mc-b",
        },
    },
}
