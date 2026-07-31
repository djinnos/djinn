# Retention gate fixtures

One directory per mutation. `scripts/test-resize-cutover-retention.mjs`
materialises a scratch root from the REAL repository (every path
`scripts/resize-cutover-retention-manifest.mjs --list-inputs` names), applies
the fixture's operations to it, and runs
`scripts/check-resize-cutover-retention.sh` against that root.

`fixture.json` fields:

| field | meaning |
| --- | --- |
| `asset` | the protected asset id this mutation targets; must appear in the gate's failure output when `expect` is `reject` |
| `expect` | `reject` (the gate must exit nonzero) or `accept` (the gate must exit 0) |
| `why` | what real-world retirement this mutation stands in for |
| `operations` | `delete` / `replace` / `append`, applied in order |

`replace` uses LITERAL text and must match exactly once. That is deliberate: a
fixture whose anchor drifted out of the source would otherwise mutate nothing
and the gate would "reject" it for no reason, or "accept" it vacuously.

The `accept-*` fixtures are not decoration. AC8 of task 1j64 requires a
**passing refactor case**: an ordinary rename inside a protected file must stay
green. That case is what makes a byte-identity formulation of this gate
impossible, which is the entire point — source files in this epic are expected
to change; what must not happen is retirement.
