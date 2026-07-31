# The image a live task-run Pod runs for fbiy-C1.
#
# It stands in for the per-project devcontainer image in exactly one respect,
# and copies it in every other: `djinn-k8s`'s renderer puts the launcher sidecar
# and the worker container on the SAME image tag and runs the launcher from
# `/opt/djinn/bin/djinn-cgroup-launcher` (`launcher::LAUNCHER_BIN`), so the
# launcher binary here lands at that exact path and the rendered sidecar command
# is left untouched. What is NOT the real image is the worker entrypoint: the
# real one is `djinn-agent-worker`, which would need a coordinator, a database
# and a model provider none of which exist on a disposable kind node.
# `governor_probe` replaces it and speaks the same broker protocol — see its
# module docs for what it does and does not decide.
#
# Debian rather than a distroless base for two reasons that are both load
# bearing: the workload is `/usr/bin/sha256sum`, a real coreutils program, and
# `/bin/sh` is the driver the broker execs. glibc 2.36 here is above the
# GLIBC_2.34 floor the release-profile binaries built on the developer host
# require (`objdump -T`), which is why the binaries are copied in rather than
# rebuilt.
FROM debian:bookworm-slim

# Both binaries come from `cargo build --release`; see build.sh, which is the
# only supported way to produce this context.
COPY djinn-cgroup-launcher /opt/djinn/bin/djinn-cgroup-launcher
COPY governor_probe /opt/djinn/bin/djinn-governor-probe

# The workload the invocation leaf hashes. Real random bytes, generated once at
# image build so every Pod in a run hashes byte-identical input and the unit
# counts are comparable across them. World readable because the brokered child
# runs as CHILD_UID 1001, which owns nothing in this image.
RUN set -eu; \
    chmod 0755 /opt/djinn/bin/djinn-cgroup-launcher /opt/djinn/bin/djinn-governor-probe; \
    head -c 33554432 /dev/urandom > /opt/djinn/workload.bin; \
    chmod 0644 /opt/djinn/workload.bin; \
    test -x /usr/bin/sha256sum

# No ENTRYPOINT and no CMD on purpose: every container in the rendered Job
# carries an explicit command, and a default here could only mask a render that
# stopped setting one.
