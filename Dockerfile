# syntax=docker/dockerfile:1
FROM rust:slim-buster as builder

WORKDIR /pandemonium

COPY Cargo.lock Cargo.toml ./
COPY ./src ./src

# Remove docker's default of removing cache after use.
RUN rm -f /etc/apt/apt.conf.d/docker-clean; echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache
ENV PACKAGES libssl-dev pkg-config
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -yqq --no-install-recommends \
    $PACKAGES && rm -rf /var/lib/apt/lists/*

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/pandemonium/target \
    cargo build --release
# Other image cannot access the target folder.
RUN --mount=type=cache,target=/pandemonium/target \
    cp ./target/release/pandemonium /usr/local/bin/pandemonium

FROM debian:buster-slim

# Remove docker's default of removing cache after use.
RUN rm -f /etc/apt/apt.conf.d/docker-clean; echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache
ENV PACKAGES libssl-dev pkg-config
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -yqq --no-install-recommends \
    $PACKAGES && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/pandemonium /bin/pandemonium

# Don't forget to also publish these ports in the docker-compose.yml file.
ARG PORT=7160

EXPOSE $PORT
ENV ROCKET_ADDRESS 0.0.0.0
ENV ROCKET_PORT $PORT

ENV RUST_LOG debug

CMD ["/bin/pandemonium"]
