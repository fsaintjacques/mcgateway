# Build memcached with proxy support.
FROM alpine:3.20 AS build

ARG MEMCACHED_VERSION=1.6.41

RUN apk add --no-cache \
        build-base \
        curl \
        libevent-dev \
        openssl-dev \
        pkgconf \
        linux-headers

WORKDIR /src
RUN curl -fsSL "https://memcached.org/files/memcached-${MEMCACHED_VERSION}.tar.gz" -o memcached.tar.gz \
 && tar -xzf memcached.tar.gz --strip-components=1 \
 && rm memcached.tar.gz

RUN ./configure --enable-proxy --prefix=/opt/memcached \
 && make -j"$(nproc)" \
 && make install

# Build libmcgateway_native.so (Lua C module, loaded at proxy init time).
# Alpine musl target matches the memcached binary's libc; undefined Lua
# symbols in the .so are resolved by the memcached process at dlopen time.
FROM rust:1.82-alpine3.20 AS rust-build

RUN apk add --no-cache build-base

WORKDIR /src/rust
COPY rust/Cargo.toml rust/Cargo.lock* ./
COPY rust/.cargo ./.cargo
COPY rust/crates ./crates
RUN cargo build --release -p mcgateway-clib

# Runtime image.
FROM alpine:3.20

RUN apk add --no-cache libevent openssl libgcc \
 && addgroup -S -g 11211 memcache \
 && adduser -S -G memcache -u 11211 memcache

COPY --from=build      /opt/memcached/bin/memcached /usr/local/bin/memcached
COPY --from=rust-build /src/rust/target/release/libmcgateway_native.so \
                       /etc/mcgateway/lua/mcgateway_native.so

RUN mkdir -p /etc/mcgateway/lua/mcgateway

COPY lua/mcgateway.lua        /etc/mcgateway/lua/mcgateway.lua
COPY lua/mcgateway/           /etc/mcgateway/lua/mcgateway/
COPY lua/examples/proxy.lua   /etc/mcgateway/proxy.lua
COPY lua/examples/config.lua  /etc/mcgateway/config.lua

USER memcache

EXPOSE 11211

CMD ["memcached", "-o", "proxy_config=/etc/mcgateway/proxy.lua", "-l", "0.0.0.0"]
