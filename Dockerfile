# ---- builder: static musl binary (rust:alpine targets musl by default) ----
FROM rust:1-alpine AS builder
# musl-dev provides the C toolchain that rustls' `ring` crypto backend compiles against.
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
# the commit this image is built from, reported by /api/version (CI passes it in)
ARG GIT_SHA=dev
RUN cargo build --release --locked

# ---- runtime: nothing but the binary (the frontend is baked into it) ----
FROM scratch
COPY --from=builder /app/target/release/sequencer /sequencer
ENV PORT=42716 DATA_DIR=/data
VOLUME /data
EXPOSE 42716
HEALTHCHECK --interval=5m --timeout=10s --start-period=30s --retries=3 \
    CMD ["/sequencer", "healthcheck"]
ENTRYPOINT ["/sequencer"]
