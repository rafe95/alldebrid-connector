FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p alldebrid-connector

FROM debian:bookworm-slim
COPY --from=build /src/target/release/alldebrid-connector /usr/local/bin/alldebrid-connector
VOLUME ["/data", "/downloads"]
EXPOSE 8080
ENTRYPOINT ["alldebrid-connector"]
