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

# Runtime image.
FROM alpine:3.20

RUN apk add --no-cache libevent openssl \
 && addgroup -S -g 11211 memcache \
 && adduser -S -G memcache -u 11211 memcache

COPY --from=build /opt/memcached/bin/memcached /usr/local/bin/memcached

RUN mkdir -p /etc/mcgateway/lua/mcgateway

COPY lua/mcgateway.lua        /etc/mcgateway/lua/mcgateway.lua
COPY lua/mcgateway/           /etc/mcgateway/lua/mcgateway/
COPY lua/examples/proxy.lua   /etc/mcgateway/proxy.lua
COPY lua/examples/config.lua  /etc/mcgateway/config.lua

USER memcache

EXPOSE 11211

CMD ["memcached", "-o", "proxy_config=/etc/mcgateway/proxy.lua", "-l", "0.0.0.0"]
