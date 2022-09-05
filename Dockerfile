FROM ekidd/rust-musl-builder:stable as builder

RUN USER=root cargo new --bin pandemonium
WORKDIR ./pandemonium

COPY Cargo.lock Cargo.toml ./

RUN cargo build --release
RUN rm src/*.rs

COPY ./src ./src

RUN rm ./target/x86_64-unknown-linux-musl/release/deps/pandemonium*
RUN cargo build --release


FROM alpine:latest

COPY --from=builder /home/rust/src/pandemonium/target/x86_64-unknown-linux-musl/release/pandemonium /bin/pandemonium

# Don't forget to also publish these ports in the docker-compose.yml file.
ARG GATEWAY_PORT=9000

EXPOSE $PORT
ENV GATEWAY_ADDRESS 0.0.0.0
ENV GATEWAY_PORT $PORT

ENV RUST_LOG debug

CMD ["/bin/pandemonium"]

