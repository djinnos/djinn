# Representative output shape of djinn-image-builder's Rust installation path.
FROM debian:trixie-slim
COPY server/crates/djinn-image-builder/scripts/ /tmp/djinn-scripts/
RUN chmod -R 0755 /tmp/djinn-scripts \
    && /tmp/djinn-scripts/base-debian.sh \
    && TOOLCHAINS="stable" DEFAULT_TOOLCHAIN="stable" COMPONENTS="clippy rustfmt" /tmp/djinn-scripts/install-rust.sh
ENV PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
