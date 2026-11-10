# Multi-stage build producing a static, distroless image.
#
# Final image: no shell, no package manager, non-root, ~2 MB base + ~4 MB binary.
# There is nothing inside it to exploit, which is rather the point for an
# exporter meant to reduce your CVE surface rather than add to it.
#
# Built for multiple architectures with:
#   docker buildx build --platform linux/amd64,linux/arm64 -t kumapulse .

FROM rust:1.99-alpine AS builder

RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/static-debian12:nonroot

LABEL org.opencontainers.image.title="kumapulse" \
      org.opencontainers.image.description="Prometheus exporter for Uptime Kuma" \
      org.opencontainers.image.source="https://github.com/daniel-gillis/kumapulse" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

COPY --from=builder /src/target/release/kumapulse /kumapulse
USER nonroot:nonroot
EXPOSE 9797
ENTRYPOINT ["/kumapulse"]
