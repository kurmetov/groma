# openRVT: the API and viewer, and the converter it shells out to.
#
# Two stages so the image carries the binaries and not the toolchain. The
# viewer page is `include_str!`d into the server at compile time, so nothing
# but the two executables is needed at runtime.

# The build stage needs a C toolchain as well as Rust: both binaries set
# `mimalloc` as their allocator, and the `rust:` image carries one.
FROM rust:1.98-bookworm AS build
WORKDIR /src

# Manifests first, so a source-only change does not re-fetch the registry.
COPY Cargo.toml Cargo.lock ./
COPY apps apps
COPY crates crates
COPY web web

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p openrvt-api -p openrvt-cli \
 && mkdir -p /out \
 && cp target/release/openrvt-api target/release/openrvt /out/

FROM debian:bookworm-slim
# The server writes uploads and scenes, and runs as nobody, so the data
# directories are owned by it rather than left to root.
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin openrvt
COPY --from=build /out/openrvt-api /out/openrvt /usr/local/bin/
RUN mkdir -p /data/scenes /data/models && chown -R openrvt:openrvt /data
USER openrvt
WORKDIR /data
EXPOSE 8800

# `--openrvt` is explicit: the server looks for the converter beside itself, and
# being told where it is survives the binaries moving.
ENTRYPOINT ["/usr/local/bin/openrvt-api", \
            "--openrvt", "/usr/local/bin/openrvt", \
            "--data", "/data/models", \
            "--scenes", "/data/scenes", \
            "--addr", "0.0.0.0:8800"]
