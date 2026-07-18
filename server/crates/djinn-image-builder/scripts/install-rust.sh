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
    # Keep mold reproducible across generated Rust images and the agent runtime.
    # The audited snapshot/version and amd64+arm64 availability are recorded in
    # docs/MOLD_DEBIAN_SNAPSHOT.md; update both installation paths together.
    readonly DEBIAN_SNAPSHOT_URL="https://snapshot.debian.org/archive/debian/20250401T000000Z"
    readonly MOLD_VERSION="2.37.1+dfsg-1"
    printf 'deb [check-valid-until=no] %s trixie main\n' "${DEBIAN_SNAPSHOT_URL}" > /etc/apt/sources.list
    rm -f /etc/apt/sources.list.d/debian.sources
    apt-get update
    apt-get install -y --no-install-recommends clang lld mold=2.37.1+dfsg-1
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

# cargo-nextest: the test runner CI uses (`cargo nextest run`). Pull the
# prebuilt static binary (no compile) into CARGO_HOME/bin so it's on the baked
# PATH. Arch-detected; best-effort so a fetch failure never fails the image.
NEXTEST_PLATFORM="linux"
case "$(uname -m)" in
    aarch64 | arm64) NEXTEST_PLATFORM="linux-arm" ;;
esac
curl --proto '=https' --tlsv1.2 -LsSf "https://get.nexte.st/latest/${NEXTEST_PLATFORM}" \
    | tar zxf - -C "${CARGO_HOME}/bin" \
    || echo "[install-rust] cargo-nextest install failed; skipping (CI uses nextest, task-runs can fall back to 'cargo test')" >&2

# cargo-sweep: prunes the warm per-project cargo target base of artifacts the
# current warm compile did NOT touch — stale crate versions cargo accumulates in
# `deps/` (it never GCs a target dir) plus orphaned `incremental/` sessions. The
# warm path brackets its compile with `cargo sweep --stamp` / `--file` so the
# base self-prunes each warm instead of growing unbounded (an un-swept base grew
# to 325G and tripped node DiskPressure). Compiled from source (small crate);
# best-effort so a build/network failure never fails the image build — the warm
# degrades to a no-op prune when the binary is absent.
"${CARGO_HOME}/bin/cargo" install --locked cargo-sweep \
    || echo "[install-rust] cargo-sweep install failed; warm-base pruning will no-op until present" >&2

# Use `:-` defaults, NOT unconditional exports: the worker runs the agent's
# shell tool as a LOGIN shell (`bash -lc`), so this fragment is sourced on
# every command. The K8s pod sets CARGO_HOME=/cache/cargo at runtime (job.rs)
# to route the registry/crate-source cache to the persistent /cache PVC; an
# unconditional `export CARGO_HOME=/usr/local/cargo` here clobbers that back to
# the ephemeral image layer, so agent-invoked cargo re-downloads crates COLD
# every run. The `:-` form keeps the baked default for plain login shells while
# letting the pod-level override survive.
#
# PATH, however, must point at the BAKED cargo/rustup proxies. Those live at the
# build-time CARGO_HOME (/usr/local/cargo/bin) regardless of the runtime cache
# override. Deriving PATH from the (overridden) CARGO_HOME resolved it to
# /cache/cargo/bin — the PVC cache dir, which holds the registry index/crate
# sources but NOT the cargo binary — so `cargo` fell off the login-shell PATH
# entirely and the agent dropped to cold, uncached `rustc` fallbacks (~12-min
# compiles; also the "cargo: command not found" worker loops). Expand
# ${CARGO_HOME} at BUILD time for the PATH line (note the unquoted heredoc);
# keep RUSTUP_HOME/CARGO_HOME runtime-overridable via escaped `\${...}`.
cat > /etc/profile.d/10-rust.sh <<EOF
export RUSTUP_HOME="\${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="\${CARGO_HOME:-/usr/local/cargo}"
export PATH="${CARGO_HOME}/bin:\${PATH}"
EOF
chmod 0644 /etc/profile.d/10-rust.sh
