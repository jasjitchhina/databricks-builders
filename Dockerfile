# Shopping Sessions: single Rust binary (fast/). Multi-stage: build, then a tiny runtime.
# pinned for a reproducible final rebuild (rustc 1.100.0, 2026-11-09)
FROM rust:1.100.0-slim-bookworm AS build
WORKDIR /src/fast
# cache deps
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/.cargo ./.cargo
RUN mkdir -p src/bin && echo 'fn main(){}' > src/main.rs && echo '' > src/lib.rs \
 && echo 'fn main(){}' > src/bin/pq2csv.rs && echo 'fn main(){}' > src/bin/prepdump.rs \
 && cargo build --release --locked --bin fast && rm -rf src
COPY fast/src ./src
RUN find src -name "*.rs" -exec touch {} + && cargo build --release --locked --bin fast && strip target/release/fast

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/fast/target/release/fast /app/fast
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
CMD ["/app/fast"]
