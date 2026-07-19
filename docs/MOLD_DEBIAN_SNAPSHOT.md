# Mold Debian snapshot contract

Both Rust image installation paths use the following Debian snapshot and exact
package version:

- snapshot URL: `https://snapshot.debian.org/archive/debian/20250401T000000Z`
- snapshot date: `2025-04-01T00:00:00Z`
- suite/component: `trixie main`
- package: `mold=2.37.1+dfsg-1`

The snapshot's `dists/trixie/main/binary-{amd64,arm64}/Packages.xz` indexes
were checked when this pin was chosen. Each contains `mold 2.37.1+dfsg-1`:

| Architecture | Package SHA-256 |
| --- | --- |
| `amd64` | `f5ac411a91bf8a2de093b349072ae6b529dffb907dd92456e3871f222bdc5ca0` |
| `arm64` | `eb1277b2e68fac00ed789be8ab74f99d8d00f684021c411ace90bfeef4f74a26` |

`server/docker/djinn-agent-runtime-base.Dockerfile` explicitly supports
`amd64` and `arm64`; the generated-image Rust installer also supports those
Debian architectures. `scripts/test-mold-debian-snapshot-contract.sh` enforces
that both paths retain this same literal pin and snapshot.
