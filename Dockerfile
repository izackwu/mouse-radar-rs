FROM rust:1.94-alpine AS builder

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig

WORKDIR /app

# Cache dependencies: copy manifests, build dummy src, compile deps
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null; true  # pre-fetch dependency crates

# Now copy real source and build
RUN rm -rf src
COPY fonts/ fonts/
COPY src/ src/
RUN cargo build --release

FROM alpine:3.21

RUN apk add --no-cache ca-certificates font-noto-cjk font-noto-emoji

WORKDIR /app
COPY --from=builder /app/target/release/mouse-radar-rs /app/mouse-radar-rs
COPY fonts/ /app/fonts/

CMD ["/app/mouse-radar-rs"]
