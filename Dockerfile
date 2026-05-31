# Build the warren hub image from this repo:  docker build -t warren .
# Runs the hub by default; override the CMD to run a node instead.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin warren

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/warren /usr/local/bin/warren
# Volume holds the TLS cert and the SQLite state file.
VOLUME ["/data"]
ENTRYPOINT ["warren"]
CMD ["hub", "--tls", "--tls-cert-dir", "/data", "--db", "/data/warren.db", \
     "--listen", "0.0.0.0:7000", "--proxy-listen", "0.0.0.0:8000", "--admin-listen", "0.0.0.0:9000"]
