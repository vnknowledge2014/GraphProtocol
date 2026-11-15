# syntax=docker/dockerfile:1.7
# Multi-stage reproducible distroless container for the GAP gateway (`WP-M7-08`, `OPS-041`, `SEC-101`).

FROM rust:1.98.1-bookworm AS builder

ARG SOURCE_DATE_EPOCH=1700000000
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} \
    TZ=UTC \
    LC_ALL=C \
    CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=always \
    RUSTFLAGS="--remap-path-prefix=/src=/gap-src"

WORKDIR /src

# Copy workspace manifests, lockfile, and compile-time asset/source directories.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY adapters ./adapters
COPY bench ./bench
COPY proto ./proto
COPY policies ./policies
COPY examples ./examples
COPY models ./models
COPY conformance ./conformance

RUN cargo build --release --locked -p gap-cli --bin gap \
    && strip /src/target/release/gap \
    && test -x /src/target/release/gap

FROM gcr.io/distroless/cc-debian12:nonroot

LABEL org.opencontainers.image.title="gap" \
      org.opencontainers.image.description="Graph Agent Protocol (GAP) typed GraphQL-profile gateway" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.source="https://github.com/graph-agent-protocol/gap"

COPY --from=builder /src/target/release/gap /usr/local/bin/gap
COPY --from=builder /src/models/manifest.toml /etc/gap/models/manifest.toml

USER nonroot:nonroot
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/gap"]
CMD ["--help"]
