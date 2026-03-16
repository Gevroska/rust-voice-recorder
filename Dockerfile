FROM rust:1.90-bookworm AS builder
WORKDIR /app

COPY server/Cargo.toml ./Cargo.toml
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release

RUN rm -rf src
COPY server/src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/recorder-server /usr/local/bin/recorder-server

ENV APP_WEB_DIR=/app/web
COPY web /app/web

EXPOSE 3000
CMD ["recorder-server"]
