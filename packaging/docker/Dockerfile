FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p loom

FROM debian:bookworm-slim
COPY --from=build /src/target/release/loom /usr/local/bin/loom
VOLUME /var/lib/loom
EXPOSE 443/tcp 443/udp
ENTRYPOINT ["loom"]
