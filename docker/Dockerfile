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

# Build libmcgateway_native.so (Lua C module, loaded at proxy init time)
# and compile the example WASM merge UDF the runtime image ships with.
# Alpine musl target matches the memcached binary's libc; undefined Lua
# symbols in the .so are resolved by the memcached process at dlopen time.
FROM rust:1.94-alpine3.20 AS rust-build

RUN apk add --no-cache build-base
# `wasm32-wasip1` is the SDK's canonical target — rustup adds it on
# demand. The build-stage is discarded after the artifacts are copied
# out, so the extra stdlib slice does not bloat the runtime image.
RUN rustup target add wasm32-wasip1

WORKDIR /src/rust
COPY rust/Cargo.toml rust/Cargo.lock* ./
COPY rust/.cargo ./.cargo
COPY rust/crates ./crates
COPY rust/examples ./examples
RUN cargo build --release -p mcgateway-clib \
 && cargo build --release --target wasm32-wasip1 -p merge-last-n-wins \
 && cargo build --release --target wasm32-wasip1 -p merge-profile-proto

# Runtime image.
FROM alpine:3.20

RUN apk add --no-cache libevent openssl libgcc \
 && addgroup -S -g 11211 memcache \
 && adduser -S -G memcache -u 11211 memcache

COPY --from=build      /opt/memcached/bin/memcached /usr/local/bin/memcached
COPY --from=rust-build /src/rust/target/release/libmcgateway_native.so \
                       /etc/mcgateway/lua/mcgateway_native.so
# Bake the example WASM merge into the image so every gateway pod has
# a real UDF to exercise on boot. Operator-managed .wasm files (Stage 4)
# will later mount here as well; the directory shape is stable.
COPY --from=rust-build /src/rust/target/wasm32-wasip1/release/merge_last_n_wins.wasm \
                       /etc/mcgateway/udf/merge_last_n_wins.wasm
COPY --from=rust-build /src/rust/target/wasm32-wasip1/release/merge_profile_proto.wasm \
                       /etc/mcgateway/udf/merge_profile_proto.wasm

RUN mkdir -p /etc/mcgateway/lua/mcgateway

COPY lua/mcgateway.lua        /etc/mcgateway/lua/mcgateway.lua
COPY lua/mcgateway/           /etc/mcgateway/lua/mcgateway/
COPY lua/examples/proxy.lua   /etc/mcgateway/proxy.lua
COPY lua/examples/config.lua  /etc/mcgateway/config.lua

USER memcache

EXPOSE 11211

CMD ["memcached", "-o", "proxy_config=/etc/mcgateway/proxy.lua", "-l", "0.0.0.0"]
