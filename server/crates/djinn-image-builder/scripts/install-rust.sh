#!/usr/bin/env bash
# Install rustup, then one or more toolchains with the requested
# components + targets.
#
# Inputs (env, space-separated):
#   TOOLCHAINS  — required. e.g. "stable" / "stable 1.85.0" / "nightly-2026-04-01".
#   COMPONENTS  — optional. e.g. "rust-analyzer clippy". Best-effort —
#                 old nightlies may not carry rust-analyzer; failure to
#                 install a component does not fail the build.
#   TARGETS     — optional. e.g. "x86_64-unknown-linux-musl".
#   DEFAULT_TOOLCHAIN — optional; defaults to the first entry in TOOLCHAINS.
#                 Used for `rustup default`.
#
# Layout: RUSTUP_HOME=/usr/local/rustup, CARGO_HOME=/usr/local/cargo.
# PATH fragment is dropped at /etc/profile.d/10-rust.sh so every
# downstream shell picks up cargo/rustup.
#
# rustup component add is idempotent, so re-warming a cached image is
# cheap — no network traffic for already-installed components.
set -euo pipefail

: "${TOOLCHAINS:?TOOLCHAINS is required (space-separated, e.g. \"stable 1.85.0\")}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
export PATH="${CARGO_HOME}/bin:${PATH}"

# Rust build toolchain beyond rustc/cargo: a real linker + the sccache wrapper
# that repos routinely pin in .cargo/config.toml (e.g. `linker = "clang"`,
# `-fuse-ld=mold`, `rustc-wrapper = "sccache"`). Without these the image only
# has gcc from build-essential, so `cargo check`/`clippy`/`build` fail at link
# time (or on the missing wrapper) and the worker can't compile or debug Rust
# at all. clang/lld/mold are required (the linker); sccache is best-effort so a
# distro without the package never fails the image build.
if command -v apt-get >/dev/null 2>&1; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends clang lld mold
    apt-get install -y --no-install-recommends sccache \
        || echo "[install-rust] sccache unavailable via apt; repos pinning rustc-wrapper=sccache may need RUSTC_WRAPPER unset" >&2
    apt-get clean
    rm -rf /var/lib/apt/lists/*
fi

DEFAULT_TOOLCHAIN_VALUE="${DEFAULT_TOOLCHAIN:-}"
if [ -z "${DEFAULT_TOOLCHAIN_VALUE}" ]; then
    # shellcheck disable=SC2086
    set -- ${TOOLCHAINS}
    DEFAULT_TOOLCHAIN_VALUE="$1"
fi

# Install rustup with the chosen default toolchain. --profile minimal
# keeps the image small; add components explicitly below.
curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${DEFAULT_TOOLCHAIN_VALUE}"

for toolchain in ${TOOLCHAINS}; do
    if [ "${toolchain}" != "${DEFAULT_TOOLCHAIN_VALUE}" ]; then
        "${CARGO_HOME}/bin/rustup" toolchain install --profile minimal "${toolchain}"
    fi
    for component in ${COMPONENTS:-}; do
        "${CARGO_HOME}/bin/rustup" component add "${component}" --toolchain "${toolchain}" \
            || echo "[install-rust] component '${component}' unavailable on '${toolchain}'; skipping" >&2
    done
    for target in ${TARGETS:-}; do
        "${CARGO_HOME}/bin/rustup" target add "${target}" --toolchain "${toolchain}" \
            || echo "[install-rust] target '${target}' unavailable on '${toolchain}'; skipping" >&2
    done
done

"${CARGO_HOME}/bin/rustup" default "${DEFAULT_TOOLCHAIN_VALUE}"

cat > /etc/profile.d/10-rust.sh <<'EOF'
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME=/usr/local/cargo
export PATH="${CARGO_HOME}/bin:${PATH}"
EOF
chmod 0644 /etc/profile.d/10-rust.sh
