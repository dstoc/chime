FROM docker.io/library/rust:1.99.1-bookworm AS build

WORKDIR /build

RUN apt-get update \
    && apt-get install --no-install-recommends -y build-essential libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY web ./web

RUN cargo build --locked --release --package chime-server

FROM docker.io/library/debian:12.15-slim AS runtime

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates libgcc-s1 libssl3 libzstd1 zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 chime \
    && useradd --system --uid 10001 --gid chime --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin chime \
    && install -d --mode 0700 --owner 10001 --group 10001 /var/lib/chime

COPY --from=build /build/target/release/chime-server /usr/local/bin/chime-server

RUN /usr/local/bin/chime-server --help >/dev/null

USER 10001:10001
WORKDIR /
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/chime-server"]
