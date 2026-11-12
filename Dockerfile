# The build stage always runs on the build machine's own CPU and cross-compiles for the
# target platform, so multi-arch builds do not need slow CPU emulation.
FROM --platform=$BUILDPLATFORM docker.io/library/rust:1-slim-bookworm AS build
ARG TARGETARCH
WORKDIR /src
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  gcc=x86_64-linux-gnu-gcc;  pkg=gcc-x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gcc=aarch64-linux-gnu-gcc; pkg=gcc-aarch64-linux-gnu ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    apt-get update; \
    apt-get install -y --no-install-recommends "$pkg" libc6-dev-${TARGETARCH}-cross; \
    rm -rf /var/lib/apt/lists/*; \
    rustup target add "$triple"; \
    var=$(echo "$triple" | tr 'a-z-' 'A-Z_'); \
    printf 'export RUST_TRIPLE=%s\nexport CARGO_TARGET_%s_LINKER=%s\nexport CC_%s=%s\n' \
      "$triple" "$var" "$gcc" "$(echo "$triple" | tr '-' '_')" "$gcc" > /cross.env
COPY . .
RUN set -eux; . /cross.env; \
    cargo build --release --locked -p alldebrid-connector --target "$RUST_TRIPLE"; \
    cp "target/$RUST_TRIPLE/release/alldebrid-connector" /alldebrid-connector

FROM docker.io/library/debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/rafe95/alldebrid-connector"
COPY --from=build /alldebrid-connector /usr/local/bin/alldebrid-connector
VOLUME ["/data", "/downloads"]
EXPOSE 8080
ENTRYPOINT ["alldebrid-connector"]
