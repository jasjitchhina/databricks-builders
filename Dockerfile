# Shopping Sessions: single Rust binary (fast/). Multi-stage: build, then a tiny runtime.
# Reproducible scorer rebuild: both base images pinned by digest (rust 1.100.0, distroless cc-debian12
# nonroot as of 2026-11-21), all crates vendored in fast/vendor and built --offline --locked, so the final
# rebuild cannot be broken by a new rust:1 tag, a moved distroless tag, or crates.io being unreachable.
# target-cpu=x86-64-v3 (never native: no AVX-512 in the binary): RUSTFLAGS below repeats fast/.cargo/config.toml's
# flag because RUSTFLAGS replaces config rustflags.
FROM rust:1.100.0-slim-bookworm@sha256:7c7d4eb4d05b7781187a327809466cfddf29753f6defdc3d19a0e291951b7a37 AS build
WORKDIR /src/fast
# cache deps (vendored, no network needed)
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/.cargo ./.cargo
COPY fast/vendor ./vendor
# PGO (task 52): profile from harness/pgo.sh (instrumented build in this same image at this same path,
# trained on s7/s11/adv sets). A stale profile only costs speed, never correctness.
COPY fast/pgo ./pgo
ENV RUSTFLAGS="-C target-cpu=x86-64-v3 -Cprofile-use=/src/fast/pgo/fast.profdata"
RUN mkdir -p src/bin && echo 'fn main(){}' > src/main.rs && echo '' > src/lib.rs \
 && echo 'fn main(){}' > src/bin/prepdump.rs \
 && cargo build --release --locked --offline --bin fast && rm -rf src
COPY fast/src ./src
RUN find src -name "*.rs" -exec touch {} + && cargo build --release --locked --offline --bin fast && strip target/release/fast

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/fast/target/release/fast /app/fast
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
CMD ["/app/fast"]
