# syntax=docker/dockerfile:1.7
FROM rust:1.86.0-bookworm@sha256:300ec56abce8cc9448ddea2172747d048ed902a3090e6b57babb2bf19f754081 AS builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo +1.86.0 build --release --locked -p dekopon-run && \
    cp target/release/dekopon-run /tmp/dekopon-run

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818
COPY --from=builder /tmp/dekopon-run /usr/local/bin/dekopon-run
COPY examples/providers/echo-provider.wasm /opt/dekopon/providers/echo-provider.wasm
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/dekopon-run"]
