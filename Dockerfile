# BootBob in one image: the static server, the web UI and the iPXE builds.
#
#   docker build -t bootbob .
#   docker run --network host --cap-add NET_RAW -v bootbob:/var/lib/bootbob \
#     -v ./bootbob.toml:/etc/bootbob/bootbob.toml:ro bootbob
#
# Host networking because TFTP, the DHCP listener and the machines' HTTP requests all need
# the real LAN interface. NET_RAW is only for the (receive-only) DHCP listener.

FROM rust:1-alpine AS server
RUN apk add --no-cache musl-dev
WORKDIR /src/backend
COPY backend/ ./
COPY ipxe/embed.ipxe /src/ipxe/embed.ipxe
RUN cargo build --locked --release -p bootbob && strip target/release/bootbob

FROM oven/bun:1 AS web
WORKDIR /src/web
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile
COPY web/ ./
RUN bun run build

FROM debian:trixie-slim AS ipxe
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential git perl liblzma-dev ca-certificates gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*
COPY ipxe/ /src/ipxe/
RUN /src/ipxe/build.sh /out

FROM alpine:3
COPY --from=server /src/backend/target/release/bootbob /usr/bin/bootbob
COPY --from=web /src/web/dist /usr/share/bootbob/web
COPY --from=ipxe /out /usr/share/bootbob/ipxe
COPY packaging/bootbob.toml /etc/bootbob/bootbob.toml
VOLUME /var/lib/bootbob
ENV BOOTBOB_CONFIG=/etc/bootbob/bootbob.toml
EXPOSE 8067/tcp 69/udp
ENTRYPOINT ["/usr/bin/bootbob"]
