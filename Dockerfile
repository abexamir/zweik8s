# syntax=docker/dockerfile:1.7
# Behind a registry mirror: --build-arg REGISTRY=mirror.example.com/library/
ARG REGISTRY=""
FROM ${REGISTRY}rust:1.100.0-alpine3.24 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo fetch --locked
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked && ./target/release/zweik8s version

FROM ${REGISTRY}alpine:3.24.4
RUN apk add --no-cache wireguard-tools-wg iproute2-minimal nftables \
 && command -v wg ip nft >/dev/null
COPY --from=build /src/target/release/zweik8s /usr/local/bin/zweik8s
LABEL org.opencontainers.image.source="https://github.com/abexamir/zweik8s" \
      org.opencontainers.image.description="Connect Services of two Kubernetes clusters over one WireGuard tunnel" \
      org.opencontainers.image.licenses="Apache-2.0"
ENTRYPOINT ["/usr/local/bin/zweik8s"]
CMD ["run"]
