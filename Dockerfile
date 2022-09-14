FROM rust:slim-buster as builder

RUN USER=root cargo new --bin pandemonium
WORKDIR /pandemonium

COPY Cargo.lock Cargo.toml ./

RUN apt-get update && apt-get install -y build-essential pkg-config libssl-dev

RUN cargo build --release
RUN rm src/*.rs

COPY ./src ./src

RUN rm ./target/release/deps/pandemonium*
RUN cargo build --release


FROM debian:buster-slim

RUN apt-get update && apt-get install -y pkg-config libssl-dev

COPY --from=builder /pandemonium/target/release/pandemonium /bin/pandemonium

# Don't forget to also publish these ports in the docker-compose.yml file.
ARG PORT=7160

EXPOSE $PORT
ENV GATEWAY_ADDRESS 0.0.0.0
ENV GATEWAY_PORT $PORT

ENV RUST_LOG debug

CMD ["/bin/pandemonium"]

