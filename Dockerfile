# Self-hosting image for the hub. Not used for local development.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p ah-hub

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/ah-hub /usr/local/bin/ah-hub
# The embedding model is downloaded to $HOME/.cache/ah-hub on first start.
ENV HOME=/data PORT=8080
VOLUME /data
EXPOSE 8080
CMD ["ah-hub", "serve"]
