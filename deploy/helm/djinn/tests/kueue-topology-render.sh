#!/usr/bin/env bash
# Structural Helm render contract for the Kueue topology and its arming switch.
#
# TWO FLAGS, NOT ONE:
#   kueue.enabled — renders the ResourceFlavor/ClusterQueue/LocalQueue topology.
#   kueue.armed   — labels the Namespace djinn.io/kueue-managed AND sets
#                   DJINN_KUEUE_ARMED on djinn-server, which is what makes the
#                   Job renderers emit `suspend: true` + a queue-name label.
#
# The pair exists so "Kueue installed, topology rendered, nothing captured" stays
# representable — the state deploy/kueue/zero-capture-gate.sh verifies. Collapsing
# them would make it unrepresentable and leave that gate with nothing to check.
#
# Usage: bash deploy/helm/djinn/tests/kueue-topology-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$CHART_DIR/../../.." && pwd)"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# The Rust crates that render the build Jobs Kueue captures. The ClusterQueue's
# coveredResources is DERIVED from these, never declared — see
# `derive_renderer_request_keys` below.
CRATES_ROOT="$REPO_ROOT/server/crates"
[ -d "$CRATES_ROOT" ] || {
    echo "FAIL: the Job renderers are not where this guard reads them: $CRATES_ROOT" >&2
    exit 1
}

render() {
    local output=$1
    shift
    helm template kueue-topology-test "$CHART_DIR" --is-upgrade "$@" >"$output"
}

# Same render, with helm's stderr folded into the output file. Rejection cases
# grep the diagnostic, and `fail` messages arrive on stderr.
render_capture() {
    local output=$1
    shift
    helm template kueue-topology-test "$CHART_DIR" --is-upgrade "$@" >"$output" 2>&1
}

# Topology ON, arming OFF — the "Kueue installed, nothing captured" state.
#
# Both flags now SHIP true, so this harness has to re-establish that state
# explicitly. `kueue.enabled=true` is redundant against the current defaults and
# is kept deliberately: it is what these cases are about, and a future default
# flip must not silently turn them into assertions about something else. The
# disarm goes FIRST so a caller that wants the armed state can simply pass
# `--set kueue.armed=true` after it — Helm applies `--set` in order and the last
# one wins.
#
# Without this, "topology rendered, nothing captured" — the state
# deploy/kueue/zero-capture-gate.sh exists to verify — would be unreachable from
# here, and every `assert_topology ... no` below would be asserting a state the
# chart can no longer produce.
render_enabled() {
    local output=$1
    shift
    render "$output" --set kueue.enabled=true --set kueue.armed=false "$@"
}

# ---------------------------------------------------------------------------
# WHAT THE RENDERERS ACTUALLY ASK FOR.
#
# Kueue refuses to assign a flavor when ANY resource a PodSet requests falls
# outside the ClusterQueue's resourceGroups ("resource cpu unavailable in
# ClusterQueue", measured live on fbiy-B1). The failure is silent and total: an
# armed install captures every build Job, suspends it, and unsuspends none —
# `helm template` renders happily, and the zero-capture gate passes precisely
# because nothing is captured while unarmed.
#
# So the coverage set cannot be a literal compared against a literal. It is
# EXTRACTED from the Rust that renders the Jobs:
#
#   1. Discover the Job renderers: every non-test `.rs` under $CRATES_ROOT that
#      defines a `pub fn build_*job*(..) -> Job` — or `-> Result<Job, E>`, since
#      a renderer that can REFUSE still emits `suspend: true` and still requests
#      resources on every path that returns one. Today that is job.rs
#      (task-run), warm_job.rs, scip_job.rs and the image-controller's
#      build_job.rs — but the LIST is derived, so a fifth renderer joins it
#      without an edit here.
#   2. Take the crates those renderers live in, and read EVERY
#      `requests: Some(BTreeMap::from([..]))` map in them. Kueue sums a PodSet's
#      request across every container in the Pod, so the launcher sidecar
#      (launcher.rs), the backing-service sidecars (sidecar.rs) and the
#      post-render override path (build_resources.rs) count exactly as much as
#      the renderer entry points do — which is why the unit of extraction is the
#      crate, not the four files.
#   3. Fail if any `requests:` site in that scope does NOT match the shape the
#      extractor parses. A renderer that builds its map some other way must not
#      be silently invisible; that is the same silence this guard exists to end.
#
# Emits JSON: {"keys": {"<resource>": ["<file>:<line>", ..]}, ..}
#
# `inject_key` is the non-vacuity handle: it appends one synthetic entry to the
# first requests map of each renderer file, in memory, so the section at the
# bottom of this file can prove the guard fails when a renderer drifts.
derive_renderer_request_keys() {
    local output=$1 inject_key=${2:-}
    python3 - "$CRATES_ROOT" "$inject_key" >"$output" <<'PY'
import json
import re
import sys
from pathlib import Path

crates_root, inject_key = Path(sys.argv[1]), sys.argv[2]


def fail(message):
    raise SystemExit(f"FAIL: renderer request extraction: {message}")


def strip_comments(text):
    """Blank out Rust comments, returning TWO length-identical forms of the file.

    Without this a doc comment mentioning `requests:` reads as a request site
    and the unparsed-shape check below fires on prose.

    Returns `(keep, blank)`:
      * `keep`  — string literals verbatim. This is what the extractor scans;
                  its resource names ARE string literals (`"cpu".to_string()`).
      * `blank` — string BODIES replaced by spaces, delimiters and newlines kept.
                  Only `strip_cfg_test` reads this, and only to count braces: a
                  `println!("{")` inside a test must not be able to unbalance the
                  brace depth and take production code down with it.

    Both forms are byte-for-byte the same length as the input, so they split into
    the same lines at the same indices and `line_of` still reports real line
    numbers.

    A trailing comment never eats the code in front of it: `foo(); // requests:`
    keeps `foo();`. And a `//` inside a string literal is not a comment, so
    `"https://x"` does not blank the rest of its line.
    """
    keep, blank, i, n = [], [], 0, len(text)
    while i < n:
        char = text[i]
        # A char literal whose value IS a double quote must not open a phantom
        # string and swallow the rest of the file. Same-length placeholder.
        if char == "'":
            for width in (3, 4):
                if text[i : i + width] in ("'\"'", "'\\\"'"):
                    placeholder = "'" + "q" * (width - 2) + "'"
                    keep.append(placeholder)
                    blank.append(placeholder)
                    i += width
                    break
            else:
                keep.append(char)
                blank.append(char)
                i += 1
            continue
        if char == '"':
            keep.append(char)
            blank.append(char)
            i += 1
            while i < n:
                if text[i] == "\\":
                    keep.append(text[i : i + 2])
                    blank.append("  ")
                    i += 2
                    continue
                keep.append(text[i])
                blank.append(text[i] if text[i] in '"\n' else " ")
                i += 1
                if text[i - 1] == '"':
                    break
            continue
        if text.startswith("//", i):
            while i < n and text[i] != "\n":
                keep.append(" ")
                blank.append(" ")
                i += 1
            continue
        if text.startswith("/*", i):
            depth = 1
            keep.append("  ")
            blank.append("  ")
            i += 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                    keep.append("  ")
                    blank.append("  ")
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                    keep.append("  ")
                    blank.append("  ")
                else:
                    keep.append("\n" if text[i] == "\n" else " ")
                    blank.append("\n" if text[i] == "\n" else " ")
                    i += 1
            continue
        keep.append(char)
        blank.append(char)
        i += 1
    return "".join(keep), "".join(blank)


# A test attribute. `#[cfg(not(test))]` deliberately does not match: it marks the
# PRODUCTION arm of a cfg split and must survive.
TEST_ATTR = re.compile(
    r"^[ \t]*#\[[ \t]*(?:cfg[ \t]*\([ \t]*test[ \t]*\)|test|tokio::test|rstest"
    r"|async_std::test|proptest)"
)
ATTR_OPEN = re.compile(r"^[ \t]*#\[")


def _after_attributes(line):
    """Whatever follows the leading `#[..]` attributes on `line`.

    `#[cfg(test)] field: Option<T>,` puts the attribute and the item it decorates
    on ONE line; without this the item would be looked for on the next line,
    which is production code.
    """
    rest = line
    while True:
        match = ATTR_OPEN.match(rest)
        if not match:
            return rest
        depth, i = 0, match.end() - 1
        while i < len(rest):
            if rest[i] == "[":
                depth += 1
            elif rest[i] == "]":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        if depth:
            return ""  # attribute spans lines; the item has not started yet
        rest = rest[i + 1 :]


def strip_cfg_test(keep, blank):
    """Blank the `#[cfg(test)]` items, and NOTHING else.

    STRUCTURAL, never positional — this is the Python counterpart of
    `rs_test_context` in `scripts/lib/rust-source-scan.awk`; read that file for
    the full rationale.

    The previous implementation scanned forward from the attribute to the next
    `{` or `;` and brace-matched from there. A struct FIELD ends in `,`, so on
    `#[cfg(test)] dispatch_image_override: Option<String>,` the scan ran past the
    end of the struct and matched the brace of the NEXT item — deleting the rest
    of the struct plus an entire production `impl` block. Measured on
    server/crates/djinn-k8s/src/runtime.rs (which this guard reads: it is the
    crate that owns job.rs) that ate 91 lines of production code, the whole of
    `impl KubernetesRuntime`. Nothing in that span happens to declare a
    `requests:` map TODAY, so the derived coverage set is unchanged — but a Job
    renderer or a `requests:` map added anywhere in it would have been invisible,
    and the ClusterQueue coverage guard would have stayed green while an
    uncovered resource shipped.

    The rule instead: a test attribute ARMS the tracker, and the item it
    decorates either opens a brace (removed to its matching close) or terminates
    on its own line (`,` for a field, `;` for a `mod x;`/`use`/`const`/`let`) and
    only that line goes. Paren depth is carried so a multi-line signature
    `fn f(\n  a: A,\n) {` still resolves to its block.

    Disarming is EAGER, matching the awk: over-keeping leaves test-only resources
    in the derived set, which makes the coverage assertion demand MORE coverage
    and fail loudly. Over-deleting is the silent direction, and it is the one
    this rewrite closes.
    """
    kept = keep.split("\n")
    code = blank.split("\n")
    depth, armed, parens = 0, False, 0
    for index, line in enumerate(code):
        if depth > 0:
            kept[index] = ""
            depth += line.count("{") - line.count("}")
            if depth < 0:
                depth = 0
            continue
        if TEST_ATTR.search(line):
            armed, parens = True, 0
            kept[index] = ""
            item = _after_attributes(line)
        elif armed:
            kept[index] = ""
            item = _after_attributes(line)
        else:
            continue
        # Stacked attributes, blank lines and (already-blanked) comment lines sit
        # between the attribute and the item it decorates.
        if not item.strip():
            continue
        parens += item.count("(") - item.count(")")
        opened = item.count("{") - item.count("}")
        if opened > 0:
            depth, armed, parens = opened, False, 0
            continue
        if parens > 0:
            continue  # still inside a multi-line signature
        armed, parens = False, 0
    return "\n".join(kept)


def _self_test_strip():
    """Both directions, on fixtures, every run.

    A stripper that silently over-deletes makes every assertion downstream
    vacuous, and the failure looks exactly like a passing guard. So the guard
    proves its own stripper before it uses it.
    """

    def prepared(source):
        return strip_cfg_test(*strip_comments(source))

    # 1. A test module goes; production AFTER it is still production.
    out = prepared(
        "pub fn keep_me() -> Job { todo!() }\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn drop_me() { requests: 1 }\n"
        "}\n"
        "pub fn after_the_tests() -> Job { todo!() }\n"
    )
    assert "keep_me" in out and "after_the_tests" in out, out
    assert "drop_me" not in out, out

    # 2. THE DEFECT. A `#[cfg(test)]` on a struct FIELD removes the field only —
    #    not the rest of the struct, and not the impl block behind it.
    out = prepared(
        "pub struct Runtime {\n"
        "    #[cfg(test)]\n"
        "    dispatch_image_override: Option<String>,\n"
        "    pending: Arc<Mutex<Pending>>,\n"
        "}\n"
        "impl Runtime {\n"
        '    pub fn requests(&self) -> u32 { requests: 1 }\n'
        "}\n"
    )
    assert "dispatch_image_override" not in out, out
    assert "pending" in out and "impl Runtime" in out and "pub fn requests" in out, out

    # 2b. Attribute and field on ONE line: the NEXT line is production.
    out = prepared(
        "pub struct S {\n"
        "    #[cfg(test)] override_me: Option<String>,\n"
        "    keep_this_field: u8,\n"
        "}\n"
    )
    assert "override_me" not in out and "keep_this_field" in out, out

    # 3. A multi-line signature resolves to its block, and code after survives.
    out = prepared(
        "#[cfg(test)]\n"
        "fn fixture(\n"
        "    a: A,\n"
        ") {\n"
        "    let x = 1;\n"
        "}\n"
        "pub fn survivor() {}\n"
    )
    assert "fixture" not in out and "let x = 1" not in out, out
    assert "survivor" in out, out

    # 4. `#[cfg(test)] mod x;` takes one line, not a file.
    out = prepared("#[cfg(test)]\nmod fixtures;\npub fn survivor() {}\n")
    assert "fixtures" not in out and "survivor" in out, out

    # 5. `#[cfg(not(test))]` is the PRODUCTION arm and must survive intact.
    out = prepared(
        "#[cfg(test)]\nlet tag = self.override_tag();\n"
        "#[cfg(not(test))]\nlet tag: Option<String> = None;\n"
    )
    assert "override_tag" not in out, out
    assert "let tag: Option<String> = None;" in out, out

    # 6. An unbalanced brace inside a STRING inside a test must not extend the
    #    removal past the test block. This is why the brace count reads the
    #    string-blanked form.
    out = prepared(
        "#[cfg(test)]\n"
        "mod tests {\n"
        '    fn f() { println!("{"); }\n'
        "}\n"
        "pub fn survivor() -> Job { todo!() }\n"
    )
    assert "survivor" in out, out

    # 7. Comment blindness: prose ABOUT `#[cfg(test)]` must not arm the stripper.
    out = prepared(
        "// this used to be #[cfg(test)] only\n"
        "pub fn survivor() -> Job { todo!() }\n"
    )
    assert "survivor" in out, out

    # 8. A trailing comment does not launder the code in front of it, and a `//`
    #    inside a string literal is not a comment.
    keep, _ = strip_comments('let u = "https://x"; requests: 1; // requests: 2\n')
    assert 'let u = "https://x"; requests: 1;' in keep, keep
    assert "requests: 2" not in keep, keep


_self_test_strip()


# A Job renderer is a function that RETURNS a Job. Nothing here names the four.
#
# "Returns a Job" includes returning one FALLIBLY. A renderer that can refuse —
# `build_image_build_job` refuses to render a Job whose build context reports a
# launcher authority protocol its own Dockerfile does not declare — still emits
# `suspend: true` and still requests resources on every path that does return,
# so it is exactly as much of this guard's business as an infallible one. Before
# `Result` was allowed here the image-build renderer silently dropped out of the
# discovered set and the floor below caught it, which is the correct direction
# to fail; matching both shapes is the fix, and `_self_test_job_builder` keeps
# the fallible shape from regressing back out of view.
JOB_BUILDER = re.compile(
    r"pub\s+fn\s+build_[A-Za-z0-9_]*job[A-Za-z0-9_]*\s*\([^{;]*?\)\s*->\s*"
    # `Job`, or any `Result<Job, E>` — including a path-qualified alias
    # (`kube::Result<Job>`) and an error type carrying its own generics
    # (`Result<Job, Foo<Bar>>`), which the lazy body plus the `\{` anchor
    # resolves to the LAST `>` before the block.
    r"(?:Job|(?:[A-Za-z0-9_]+\s*::\s*)*Result\s*<\s*Job\s*(?:,[^{;]*?)?>)"
    r"\s*\{",
    re.S,
)


def _self_test_job_builder():
    """The discovery regex, both directions, every run.

    A discovery that quietly matches fewer renderers makes the coverage
    assertion cover less while still passing — the same silence this guard
    exists to end — so it is proven before it is used.
    """
    for source in [
        "pub fn build_task_run_job(spec: &Spec) -> Job {",
        "pub fn build_image_build_job(c: &C) -> Result<Job, DeclarationError> {",
        "pub fn build_warm_job(\n    a: A,\n    b: B,\n) -> Result<Job, Error> {",
        "pub fn build_scip_job(a: A) -> kube::Result<Job> {",
        "pub fn build_x_job(a: A) -> Result<Job, foo::Bar<Baz>> {",
        "pub fn build_y_job(a: A) -> Result< Job , E > {",
    ]:
        assert JOB_BUILDER.search(source), f"renderer not discovered: {source!r}"

    for source in [
        # Returns something that merely mentions a Job.
        "pub fn build_job_name(a: A) -> String {",
        "pub fn build_job_owner_reference(job: &Job) -> Option<OwnerReference> {",
        # A Result of something else entirely.
        "pub fn build_a_job(a: A) -> Result<ConfigMap, E> {",
        # A declaration, not a definition.
        "pub fn build_task_run_job(spec: &Spec) -> Job;",
    ]:
        assert not JOB_BUILDER.search(source), f"falsely discovered: {source!r}"


_self_test_job_builder()
REQUESTS_BLOCK = re.compile(
    r"requests\s*:\s*Some\(\s*BTreeMap::from\(\s*\[(?P<body>[^\[\]]*)\]\s*\)\s*\)", re.S
)
REQUESTS_TOKEN = re.compile(r"\brequests\s*:")
KEY = re.compile(r'\(\s*"(?P<key>[^"]+)"\s*\.to_string\(\)\s*,')


def crate_of(path):
    return crates_root / path.relative_to(crates_root).parts[0]


def line_of(text, index):
    return text.count("\n", 0, index) + 1


# Two path heuristics, both deliberate, both UNDER-reporting by construction:
#   * `*/src/**/*.rs` reaches one crate level under $CRATES_ROOT. Every Job
#     renderer in the workspace lives there today, and a renderer moved outside
#     it would take `renderer_files` below 4 and fail this extraction loudly
#     rather than silently shrink the derived set.
#   * `_tests.rs` files are declared by a parent as `#[cfg(test)] mod x;`, so the
#     attribute that makes them test code is not IN them and nothing here could
#     see it (the `force_test=1` case in scripts/lib/rust-source-scan.awk). The
#     name is the only available signal.
sources = sorted(
    path for path in crates_root.glob("*/src/**/*.rs") if not path.name.endswith("_tests.rs")
)
if not sources:
    fail(f"no Rust sources under {crates_root}")
# Comment-stripping the whole workspace costs seconds for nothing: a file that
# contains neither substring can hold neither a Job renderer nor a requests map,
# and both patterns below require them literally.
prepared = {}
for path in sources:
    text = path.read_text(encoding="utf-8")
    if "requests" not in text and "Job" not in text:
        text = ""
    else:
        text = strip_cfg_test(*strip_comments(text))
    prepared[path] = text

renderer_files = [path for path, text in prepared.items() if JOB_BUILDER.search(text)]
# The task-run, warm, SCIP and image-build renderers. Fewer than four means the
# discovery regex stopped matching, not that a renderer disappeared — and a
# discovery that finds nothing would make every assertion below vacuous.
if len(renderer_files) < 4:
    fail(
        "expected at least the four Job renderers (task-run, warm, SCIP, image build), found "
        f"{[str(p) for p in renderer_files]}"
    )
renderer_crates = {crate_of(path) for path in renderer_files}

keys, sites, unparsed, blank = {}, [], [], []
for path in sources:
    if crate_of(path) not in renderer_crates:
        continue
    text = prepared[path]
    if inject_key and path in renderer_files:
        text = REQUESTS_BLOCK.sub(
            lambda match: match.group(0).replace(
                match.group("body"),
                match.group("body")
                + f'("{inject_key}".to_string(), Quantity("1".to_string())),',
            ),
            text,
            count=1,
        )
    blocks = list(REQUESTS_BLOCK.finditer(text))
    starts = {block.start() for block in blocks}
    for token in REQUESTS_TOKEN.finditer(text):
        if token.start() not in starts:
            unparsed.append(f"{path}:{line_of(text, token.start())}")
    if not blocks:
        continue
    sites.append(str(path))
    for block in blocks:
        found = KEY.findall(block.group("body"))
        if not found:
            blank.append(f"{path}:{line_of(text, block.start())}")
        for key in found:
            keys.setdefault(key, []).append(f"{path.name}:{line_of(text, block.start())}")

if unparsed:
    fail(
        "these `requests:` sites do not match the shape this extractor parses, so the resources "
        f"they ask for cannot be derived and coverage would silently drift: {unparsed}"
    )
if blank:
    fail(f"these requests maps parsed to no resource keys at all: {blank}")
if not keys:
    fail(
        f"no resource requests found in {sorted(str(c) for c in renderer_crates)}; every "
        "assertion built on this set would be vacuous"
    )

json.dump(
    {
        "renderer_files": sorted(str(path) for path in renderer_files),
        "request_sites": sorted(sites),
        "keys": {key: sorted(set(where)) for key, where in sorted(keys.items())},
    },
    sys.stdout,
    indent=2,
)
PY
}

echo "=== deriving the resources the Job renderers request ==="
RENDERER_KEYS="$WORK/renderer-request-keys.json"
derive_renderer_request_keys "$RENDERER_KEYS"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("renderers:", *sorted(d["renderer_files"]), sep="\n  "); print("request sites:", *d["request_sites"], sep="\n  "); print("requested resources:", ", ".join(d["keys"]))' "$RENDERER_KEYS"

# `expected_managed` is yes/no: whether the Namespace must carry
# djinn.io/kueue-managed=true. It is the arming half of the contract, asserted in
# BOTH directions so neither branch can rot into a tautology.
#
# `keys_json` is the derived renderer request set; it defaults to the real one
# and is overridden only by the non-vacuity section at the bottom.
assert_topology() {
    local manifest=$1 expected_cpu=$2 expected_memory=$3 expected_pods=$4 expected_managed=$5 keys_json=${6:-$RENDERER_KEYS}
    python3 - "$manifest" "$expected_cpu" "$expected_memory" "$expected_pods" "$expected_managed" "$keys_json" <<'PY'
import json
import sys
import yaml

manifest, expected_cpu, expected_memory, expected_pods, expected_managed, keys_json = sys.argv[1:]
task_vector = {"cpu": expected_cpu, "memory": expected_memory, "pods": int(expected_pods)}
singleton_borrowing = {
    "kueue-topology-test-djinn-warm": {"cpu": "2", "memory": "2Gi", "pods": 1},
    "kueue-topology-test-djinn-scip": {"cpu": "1", "memory": "4Gi", "pods": 1},
}
derived = json.load(open(keys_json, encoding="utf-8"))
docs = [doc for doc in yaml.safe_load_all(open(manifest, encoding="utf-8")) if doc]
def named(kind):
    return [doc for doc in docs if doc.get("kind") == kind]

flavors = named("ResourceFlavor")
assert len(flavors) == 1, f"expected one ResourceFlavor, got {len(flavors)}"
flavor = flavors[0]
assert flavor.get("spec") == {}, "ResourceFlavor must be empty"

queues = named("ClusterQueue")
assert len(queues) == 3, f"expected task-run, warm and scip ClusterQueues, got {len(queues)}"
queue_by_name = {queue["metadata"]["name"]: queue for queue in queues}
for queue_name, candidate in queue_by_name.items():
    for entry in candidate["spec"]["resourceGroups"][0]["flavors"][0].get("resources", []):
        for field in ("nominalQuota", "borrowingLimit"):
            assert entry.get(field) not in ("100Ti", "10k", 10000, "10000"), (
                f"{queue_name} {entry['name']} {field} contains a legacy quota sentinel: "
                f"{entry.get(field)!r}"
            )
assert set(queue_by_name) == {
    "kueue-topology-test-djinn-kueue",
    "kueue-topology-test-djinn-warm",
    "kueue-topology-test-djinn-scip",
}, f"unexpected ClusterQueue set: {sorted(queue_by_name)}"
queue = queue_by_name["kueue-topology-test-djinn-kueue"]
spec = queue["spec"]
cohort = spec.get("cohort")
assert cohort and all(q["spec"].get("cohort") == cohort for q in queues), "all queues must share one cohort"
assert spec.get("preemption") == {"reclaimWithinCohort": "Never"}, (
    "kueue-topology-test-djinn-kueue must let admitted background work run to completion "
    f"with reclaimWithinCohort Never, got {spec.get('preemption')}"
)
# BestEffortFIFO, not StrictFIFO: three kinds share a 3-slot queue, so one
# head-of-line Workload that cannot fit would block everything behind it.
assert spec.get("queueingStrategy") == "BestEffortFIFO", (
    f"ClusterQueue must use BestEffortFIFO, got {spec.get('queueingStrategy')}"
)
groups = spec.get("resourceGroups")
assert isinstance(groups, list) and len(groups) == 1, "ClusterQueue must have one resource group"
group = groups[0]
# WHO MAY SUBMIT. An omitted namespaceSelector is not "unrestricted", it is
# "nobody": the CRD documents "Defaults to null which is a nothing selector (no
# namespaces eligible)". MEASURED on a live armed cluster (fbiy-B1) with the
# field absent, every captured Workload sat Pending with "workload namespace
# doesn't match ClusterQueue selector" and no armed Job was ever unsuspended.
assert "namespaceSelector" in spec, (
    "ClusterQueue must declare a namespaceSelector or Kueue admits nothing at all"
)
assert spec.get("namespaceSelector") == {}, (
    "the admission fence is the namespace label plus the LocalQueue's placement, not a third "
    f"copy of it here; got {spec.get('namespaceSelector')}"
)

# COVERAGE IS A SUPERSET OF WHAT THE RENDERERS REQUEST — DERIVED, NOT DECLARED.
#
# `pods` is the only intended bound, but Kueue refuses to assign a flavor when
# ANY resource a PodSet requests falls outside the ClusterQueue's
# resourceGroups: "couldn't assign flavors to pod set main: resource cpu
# unavailable in ClusterQueue" (measured live, fbiy-B1). A pods-only
# ClusterQueue therefore admits nothing at all.
#
# The set on the right-hand side comes from `derive_renderer_request_keys`,
# which reads the Job renderers' own `resources.requests` maps. Nothing here
# names a resource: the day a renderer adds `ephemeral-storage`, this assertion
# fails naming it instead of an armed install wedging in silence.
covered = group.get("coveredResources")
assert isinstance(covered, list), f"coveredResources must be a list, got {covered!r}"
requested = derived["keys"]
missing = [key for key in requested if key not in covered]
assert not missing, (
    f"the ClusterQueue does not cover {missing}, which the Job renderers request at "
    f"{ {key: requested[key] for key in missing} }. Kueue cannot assign a flavor to a PodSet "
    "asking for an uncovered resource, so an armed install would capture every build Job, "
    f"suspend it and unsuspend none. coveredResources={covered}"
)
flavor_quotas = group.get("flavors")
assert isinstance(flavor_quotas, list) and len(flavor_quotas) == 1, "exactly one flavor quota is required"
assert flavor_quotas[0].get("name") == flavor["metadata"]["name"], "quota must use the empty flavor"
resources = {entry["name"]: entry for entry in flavor_quotas[0].get("resources", [])}
assert set(resources) == set(covered), (
    f"every covered resource needs a quota and vice versa; covered={sorted(covered)} "
    f"quota={sorted(resources)}"
)
for dimension, declared in task_vector.items():
    actual = resources[dimension].get("nominalQuota")
    assert actual == declared, (
        f"{queue['metadata']['name']} {dimension} nominalQuota must equal declared "
        f"task vector {declared!r}, got {actual!r}"
    )
    assert "borrowingLimit" not in resources[dimension], (
        f"{queue['metadata']['name']} {dimension} must own nominal capacity, not borrow it"
    )

for background_name in ["kueue-topology-test-djinn-warm", "kueue-topology-test-djinn-scip"]:
    background = queue_by_name[background_name]
    background_spec = background["spec"]
    assert "preemption" not in background_spec, f"{background_name} must not preempt"
    assert "stopPolicy" not in background_spec, f"{background_name} must not set stopPolicy"
    assert background_spec.get("namespaceSelector") == {}, f"{background_name} selector drifted"
    assert background_spec.get("queueingStrategy") == "BestEffortFIFO"
    bg_groups = background_spec.get("resourceGroups")
    assert isinstance(bg_groups, list) and len(bg_groups) == 1
    assert bg_groups[0].get("coveredResources") == covered
    bg_entries = bg_groups[0]["flavors"][0]["resources"]
    bg_resources = {entry["name"]: entry for entry in bg_entries}
    assert set(bg_resources) == set(covered)
    for dimension, expected_borrowing in singleton_borrowing[background_name].items():
        actual_nominal = bg_resources[dimension].get("nominalQuota")
        actual_borrowing = bg_resources[dimension].get("borrowingLimit")
        assert actual_nominal == 0, (
            f"{background_name} {dimension} nominalQuota must be zero, got {actual_nominal!r}"
        )
        assert actual_borrowing == expected_borrowing, (
            f"{background_name} {dimension} borrowingLimit must match its singleton Job "
            f"request {expected_borrowing!r}, got {actual_borrowing!r}"
        )

for dimension, declared in task_vector.items():
    cohort_nominal = sum(
        next(
            entry["nominalQuota"]
            for entry in candidate["spec"]["resourceGroups"][0]["flavors"][0]["resources"]
            if entry["name"] == dimension
        )
        for candidate in queues
    )
    assert cohort_nominal == declared, (
        f"cohort nominal sum for {dimension} must equal declared task vector {declared!r}; "
        f"got {cohort_nominal!r} across {[candidate['metadata']['name'] for candidate in queues]}"
    )

assert "withinClusterQueue" not in spec.get("preemption", {})
assert "stopPolicy" not in spec

# Exactly three LocalQueues. SCIP joins Kueue because rust-analyzer indexing is
# CPU-heavy and excluding it under-counts the load the quota exists to bound.
# Image build stays out on purpose: it is an UPSTREAM DEPENDENCY of a task-run,
# so a shared ClusterQueue admits a priority-inversion deadlock. A fourth queue
# here is a regression, not an addition.
local_queues = named("LocalQueue")
assert len(local_queues) == 3, f"expected task-run, warm and scip LocalQueues, got {len(local_queues)}"
local_names = {local["metadata"]["name"] for local in local_queues}
assert local_names == {
    "kueue-topology-test-djinn-task-run",
    "kueue-topology-test-djinn-warm",
    "kueue-topology-test-djinn-scip",
}, f"unexpected LocalQueue set: {sorted(local_names)}"
local_targets = {
    local["metadata"]["name"]: local.get("spec", {}).get("clusterQueue")
    for local in local_queues
}
assert local_targets == {
    "kueue-topology-test-djinn-task-run": "kueue-topology-test-djinn-kueue",
    "kueue-topology-test-djinn-warm": "kueue-topology-test-djinn-warm",
    "kueue-topology-test-djinn-scip": "kueue-topology-test-djinn-scip",
}, f"LocalQueue targets drifted: {local_targets}"

namespaces = named("Namespace")
assert len(namespaces) == 1, f"expected one Namespace, got {len(namespaces)}"
labels = namespaces[0].get("metadata", {}).get("labels", {})
managed = labels.get("djinn.io/kueue-managed")
if expected_managed == "yes":
    # Kueue captures Jobs ONLY in a labelled namespace. Without this label the
    # `suspend: true` the same flag turns on is never undone, and every build
    # Job hangs forever.
    assert managed == "true", (
        f"kueue.armed=true must label the Namespace djinn.io/kueue-managed=true, got {managed!r}"
    )
else:
    assert "djinn.io/kueue-managed" not in labels, (
        "Namespace must remain unlabelled for Kueue unless kueue.armed=true"
    )

roles = named("Role")
assert len(roles) >= 1, "expected namespaced controller Role"
workload_rules = [
    (role, rule) for role in roles for rule in role.get("rules", [])
    if rule.get("apiGroups") == ["kueue.x-k8s.io"] and rule.get("resources") == ["workloads"]
]
assert len(workload_rules) == 1, f"expected exactly one Workload RBAC rule, got {len(workload_rules)}"
workload_role, workload_rule = workload_rules[0]
assert workload_role.get("metadata", {}).get("namespace") == "djinn", "Workload RBAC must remain namespaced"
assert workload_rule.get("verbs") == ["get", "list", "watch"], "Workloads must be observation-only"

for doc in docs:
    if doc.get("kind") not in {"Role", "ClusterRole", "RoleBinding", "ClusterRoleBinding"}:
        continue
    for rule in doc.get("rules", []):
        forbidden = {resource.lower().split("/", 1)[0] for resource in rule.get("resources", [])} & {"nodes", "persistentvolumes"}
        assert not forbidden, f"forbidden cluster resource RBAC: {forbidden}"

server_deployment = next(
    deployment for deployment in named("Deployment")
    if deployment.get("metadata", {}).get("name", "").endswith("-server")
)
containers = server_deployment["spec"]["template"]["spec"]["containers"]
server = next(container for container in containers if container["name"] == "djinn-server")
values = {entry["name"]: entry.get("value") for entry in server["env"]}
# Build admission is gone: the pods-quota reservation authority was deleted, so
# the server must receive neither of its env vars. Kueue's pods quota is the
# only remaining build-concurrency bound, and it is deliberately independent.
# See deploy/helm/djinn/tests/build-admission-topology.sh.
assert "DJINN_BUILD_ADMISSION_MODE" not in values, "removed build-admission env var came back"
assert "DJINN_MAX_BUILD_TASKRUNS" not in values, "removed build-admission cap env var came back"
# The renderer half of the arming contract. It must move with the namespace
# label, never independently: they are two halves of one capture decision.
expected_armed = "true" if expected_managed == "yes" else "false"
assert values["DJINN_KUEUE_ARMED"] == expected_armed, (
    f"DJINN_KUEUE_ARMED must be {expected_armed}, got {values.get('DJINN_KUEUE_ARMED')!r}"
)
# An armed Job's queue-name label has to resolve to a LocalQueue this release
# actually rendered, or it is never admitted.
assert values["DJINN_KUEUE_LOCAL_QUEUE_PREFIX"] == "kueue-topology-test-djinn", (
    f"queue prefix must match the rendered LocalQueue names, got {values.get('DJINN_KUEUE_LOCAL_QUEUE_PREFIX')!r}"
)
PY
}

echo "=== zero-capture-representable state: enabled=true, armed=false ==="
# This is the configuration deploy/kueue/zero-capture-gate.sh exists to verify:
# the whole topology present, the Namespace still unlabelled, nothing captured.
render_enabled "$WORK/valid.yaml" --set kueue.buildCpu=15 --set kueue.buildMemory=60Gi --set kueue.buildPods=7
assert_topology "$WORK/valid.yaml" 15 60Gi 7 no

echo "=== armed state: enabled=true, armed=true ==="
render_enabled "$WORK/armed.yaml" --set kueue.buildCpu=15 --set kueue.buildMemory=60Gi --set kueue.buildPods=7 --set kueue.armed=true
assert_topology "$WORK/armed.yaml" 15 60Gi 7 yes

# ---------------------------------------------------------------------------
# THE COVERAGE CHECK MUST FAIL IN BOTH DIRECTIONS, INDEPENDENTLY.
#
# A superset assertion between two sets that never move proves nothing. Move
# each side in turn, against the REAL render and the REAL renderer sources, and
# require the guard to fail NAMING the resource that drifted.
# ---------------------------------------------------------------------------
expect_topology_rejected() {
    local label=$1 manifest=$2 expected_cpu=$3 expected_memory=$4 expected_pods=$5 expected_managed=$6 keys_json=$7 must_name=$8
    local out="$WORK/nonvacuity-$label.out"
    set +e
    assert_topology "$manifest" "$expected_cpu" "$expected_memory" "$expected_pods" "$expected_managed" "$keys_json" >"$out" 2>&1
    local status=$?
    set -e
    if [ "$status" -eq 0 ]; then
        echo "FAIL: the coverage guard accepted '$label'; it does not detect drift" >&2
        exit 1
    fi
    grep -q -- "$must_name" "$out" || {
        echo "FAIL: '$label' failed without naming '$must_name', so the failure is unattributable:" >&2
        cat "$out" >&2
        exit 1
    }
}

echo "=== a renderer that requests an uncovered resource FAILS the guard ==="
# `ephemeral-storage` is the realistic instance; the probe below uses a name no
# chart could ever legitimately cover, so this stays a mutation and never
# becomes a fact about the chart.
DRIFT_PROBE="djinn.io/uncovered-drift-probe"
derive_renderer_request_keys "$WORK/renderer-keys-drifted.json" "$DRIFT_PROBE"
expect_topology_rejected renderer-drift "$WORK/valid.yaml" 15 60Gi 7 no \
    "$WORK/renderer-keys-drifted.json" "$DRIFT_PROBE"

echo "=== a ClusterQueue that stops covering a requested resource FAILS the guard ==="
# The other half, moved alone: the renderers are untouched and the chart drops
# one resource it really does have to cover. Which resource is itself derived —
# naming one here would put the literal straight back.
python3 - "$WORK/valid.yaml" "$RENDERER_KEYS" "$WORK/coverage-dropped.yaml" \
    >"$WORK/dropped-resource.txt" <<'PY'
import json
import sys
import yaml

source, keys_json, destination = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(source, encoding="utf-8")) if doc]
requested = list(json.load(open(keys_json, encoding="utf-8"))["keys"])

queue = next(doc for doc in docs if doc.get("kind") == "ClusterQueue")
group = queue["spec"]["resourceGroups"][0]
victim = next(key for key in requested if key in group["coveredResources"])
group["coveredResources"] = [key for key in group["coveredResources"] if key != victim]
for flavor in group["flavors"]:
    flavor["resources"] = [entry for entry in flavor["resources"] if entry["name"] != victim]

with open(destination, "w", encoding="utf-8") as output:
    yaml.safe_dump_all(docs, output)
sys.stdout.write(victim)
PY
expect_topology_rejected coverage-dropped "$WORK/coverage-dropped.yaml" 15 60Gi 7 no \
    "$RENDERER_KEYS" "$(cat "$WORK/dropped-resource.txt")"

echo "=== chart defaults render the topology AND arm it ==="
# The stock profile is the production profile. Kueue's pods quota is the only
# remaining build-concurrency bound (the in-process reservation authority was
# deleted in #2822), so a default that rendered no topology would ship an
# unbounded fleet. `buildPods` ships 3, and the whole armed contract — Namespace
# label included — is asserted here rather than only under an explicit `--set`.
#
# The prerequisite this creates is enforced where it can actually be checked:
# templates/prereq-guard.yaml refuses an install against a cluster that does not
# serve kueue.x-k8s.io/v1beta1, naming deploy/helm/djinn-prereqs. See
# tests/prereq-guard-render.sh.
render "$WORK/default.yaml"
assert_topology "$WORK/default.yaml" 12 48Gi 3 yes

echo "=== explicitly disabled omits the topology ==="
render "$WORK/disabled.yaml" --set kueue.enabled=false --set kueue.armed=false --set kueue.buildPods=7
grep -q 'kueue.x-k8s.io/v1beta1' "$WORK/disabled.yaml" && {
    echo "FAIL: kueue.enabled=false still rendered the v1beta1 topology" >&2
    exit 1
}
python3 - "$WORK/disabled.yaml" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
# Non-vacuity: the render must still be a real chart render, not an empty file
# that trivially satisfies the grep above.
assert any(doc.get("kind") == "Deployment" for doc in docs), (
    "the disabled render produced no Deployment; the assertion above proved nothing"
)
# The controller's observation-only Workload RBAC is deliberately NOT gated:
# it is inert without the CRDs and must not churn with the flag.
rules = [
    rule for doc in docs if doc.get("kind") == "Role"
    for rule in doc.get("rules", [])
    if rule.get("apiGroups") == ["kueue.x-k8s.io"]
]
assert len(rules) == 1, f"expected the Workload RBAC rule to survive, got {rules}"
PY

# ---------------------------------------------------------------------------
# The topology flag alone arms nothing.
# ---------------------------------------------------------------------------
assert_disarmed_server() {
    local manifest=$1 label=$2
    python3 - "$manifest" "$label" <<'PY'
import sys
import yaml

manifest, label = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(manifest, encoding="utf-8")) if doc]

namespaces = [doc for doc in docs if doc.get("kind") == "Namespace"]
assert len(namespaces) == 1, f"{label}: expected one Namespace, got {len(namespaces)}"
labels = namespaces[0].get("metadata", {}).get("labels", {})
assert "djinn.io/kueue-managed" not in labels, (
    f"{label}: the topology flag must not label the Namespace"
)

server = next(
    container
    for doc in docs
    if doc.get("kind") == "Deployment"
    and doc.get("metadata", {}).get("name", "").endswith("-server")
    for container in doc["spec"]["template"]["spec"]["containers"]
    if container["name"] == "djinn-server"
)
values = {entry["name"]: entry.get("value") for entry in server["env"]}
assert values["DJINN_KUEUE_ARMED"] == "false", (
    f"{label}: the topology flag must not arm the renderers, got "
    f"{values.get('DJINN_KUEUE_ARMED')!r}"
)
PY
}

# Same assertion for an externally-managed namespace, where there is no
# Namespace object to check at all.
assert_disarmed_server_no_namespace() {
    local manifest=$1 label=$2
    python3 - "$manifest" "$label" <<'PY'
import sys
import yaml

manifest, label = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(manifest, encoding="utf-8")) if doc]

assert not [doc for doc in docs if doc.get("kind") == "Namespace"], (
    f"{label}: namespace.create=false must render no Namespace object"
)
server = next(
    container
    for doc in docs
    if doc.get("kind") == "Deployment"
    and doc.get("metadata", {}).get("name", "").endswith("-server")
    for container in doc["spec"]["template"]["spec"]["containers"]
    if container["name"] == "djinn-server"
)
values = {entry["name"]: entry.get("value") for entry in server["env"]}
assert values["DJINN_KUEUE_ARMED"] == "false", (
    f"{label}: expected a disarmed server, got {values.get('DJINN_KUEUE_ARMED')!r}"
)
PY
}

echo "=== kueue.enabled alone arms nothing, at BOTH of its values ==="
assert_disarmed_server "$WORK/disabled.yaml" "kueue.enabled=false"
assert_disarmed_server "$WORK/valid.yaml" "kueue.enabled=true"

# ---------------------------------------------------------------------------
# armed implies enabled
# ---------------------------------------------------------------------------
echo "=== armed=true with enabled=false is rejected by name ==="
if render_capture "$WORK/armed-without-enabled.out" --set kueue.enabled=false --set kueue.armed=true; then
    echo "FAIL: kueue.armed=true with kueue.enabled=false rendered successfully" >&2
    exit 1
fi
# The invariant must be NAMED, not merely violated by some unrelated error.
grep -q 'kueue.armed=true requires kueue.enabled=true' "$WORK/armed-without-enabled.out" || {
    echo "FAIL: the armed-implies-enabled rejection did not name the invariant:" >&2
    cat "$WORK/armed-without-enabled.out" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# RuntimeClass stacking: mirror of the renderer assert at job.rs:242.
# ---------------------------------------------------------------------------
echo "=== armed=true with a required cgroup launcher and no RuntimeClass is rejected ==="
RUNTIME_CLASS_VALUES=(
    --set kueue.enabled=true
    --set-string cgroupLauncher.mode=required
    --set cgroupWritable.taskRuns.enabled=false
)
if render_capture "$WORK/armed-runtimeclass.out" "${RUNTIME_CLASS_VALUES[@]}" --set kueue.armed=true; then
    echo "FAIL: arming Kueue over an unsatisfiable cgroup launcher rendered successfully" >&2
    exit 1
fi
grep -q 'cgroupLauncher.mode=required requires cgroupWritable.taskRuns.enabled=true' "$WORK/armed-runtimeclass.out" || {
    echo "FAIL: the RuntimeClass rejection did not mirror the job.rs:242 assertion:" >&2
    cat "$WORK/armed-runtimeclass.out" >&2
    exit 1
}
# Non-vacuity: the coherent explicitly-disabled launcher profile still renders.
# HMI6 makes required+no-RuntimeClass globally invalid, independent of Kueue.
render "$WORK/disarmed-runtimeclass.yaml" \
    --set kueue.enabled=true \
    --set-string cgroupLauncher.mode=disabled \
    --set-string imagePipeline.controller.launcherAuthorityProtocol=leaf-v1 \
    --set cgroupWritable.taskRuns.enabled=false \
    --set kueue.armed=false
assert_topology "$WORK/disarmed-runtimeclass.yaml" 12 48Gi 3 no

# ---------------------------------------------------------------------------
# Arming requires a namespace the chart can actually label.
#
# `namespace.create: false` renders no Namespace object, so the chart CANNOT
# apply djinn.io/kueue-managed while still setting DJINN_KUEUE_ARMED. Every
# build Job would render `suspend: true` into a namespace Kueue does not manage,
# nothing would capture them, and nothing would ever unsuspend them. That is the
# exact failure the one-flag design exists to prevent, reachable through a values
# combination the chart used to accept in silence.
# ---------------------------------------------------------------------------
echo "=== armed=true with namespace.create=false is rejected by name ==="
if render_capture "$WORK/armed-external-ns.out" \
    --set kueue.enabled=true --set kueue.armed=true --set namespace.create=false; then
    echo "FAIL: kueue.armed=true with namespace.create=false rendered successfully" >&2
    exit 1
fi
# The hazard must be NAMED, not merely violated by some unrelated render error.
grep -q 'kueue.armed=true with namespace.create=false cannot label the namespace' \
    "$WORK/armed-external-ns.out" || {
    echo "FAIL: the unlabelled-namespace rejection did not name the hazard:" >&2
    cat "$WORK/armed-external-ns.out" >&2
    exit 1
}
grep -q 'djinn.io/kueue-managed' "$WORK/armed-external-ns.out" || {
    echo "FAIL: the rejection did not name the label Kueue actually requires:" >&2
    cat "$WORK/armed-external-ns.out" >&2
    exit 1
}
# The gate lives in templates/namespace.yaml, i.e. in the template that renders
# NOTHING in this configuration. Prove it still fires when helm is asked for one
# unrelated template, because `--show-only` filters after rendering.
if helm template kueue-topology-test "$CHART_DIR" --is-upgrade \
    --set kueue.enabled=true --set kueue.armed=true --set namespace.create=false \
    --show-only templates/deployment-server.yaml >"$WORK/armed-external-ns-showonly.out" 2>&1; then
    echo "FAIL: the unlabelled-namespace gate did not fire under --show-only" >&2
    exit 1
fi
grep -q 'kueue.armed=true with namespace.create=false cannot label the namespace' \
    "$WORK/armed-external-ns-showonly.out" || {
    echo "FAIL: --show-only skipped the gate in templates/namespace.yaml:" >&2
    cat "$WORK/armed-external-ns-showonly.out" >&2
    exit 1
}

echo "=== namespace.create=false alone still renders; only ARMING is gated ==="
# Non-vacuity: an externally-managed namespace is a supported configuration and
# must keep rendering. Without this the rejection above could be an unrelated
# namespace.create=false breakage.
render "$WORK/external-ns-disarmed.yaml" --set kueue.enabled=true --set kueue.armed=false --set namespace.create=false
assert_disarmed_server_no_namespace "$WORK/external-ns-disarmed.yaml" "namespace.create=false, armed=false"

echo "=== the acknowledgement reopens the escape hatch ==="
# The gate must not be an unconditional fail: an operator who HAS labelled their
# externally-managed namespace can still arm.
render "$WORK/external-ns-acked.yaml" \
    --set kueue.enabled=true --set kueue.armed=true --set namespace.create=false \
    --set kueue.namespaceLabelledExternally=true
python3 - "$WORK/external-ns-acked.yaml" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]

# No Namespace object: that is the whole premise of this configuration.
assert not [doc for doc in docs if doc.get("kind") == "Namespace"], (
    "namespace.create=false must still render no Namespace object"
)
# The topology must be there for the armed Jobs to be admitted into.
local_queues = [doc for doc in docs if doc.get("kind") == "LocalQueue"]
assert len(local_queues) == 3, f"expected three LocalQueues, got {len(local_queues)}"

server = next(
    container
    for doc in docs
    if doc.get("kind") == "Deployment"
    and doc.get("metadata", {}).get("name", "").endswith("-server")
    for container in doc["spec"]["template"]["spec"]["containers"]
    if container["name"] == "djinn-server"
)
values = {entry["name"]: entry.get("value") for entry in server["env"]}
# The escape hatch must produce a genuinely ARMED render — otherwise the
# acknowledgement would be a no-op that only silenced the gate.
assert values["DJINN_KUEUE_ARMED"] == "true", (
    "the acknowledgement must still arm the renderers, got "
    f"{values.get('DJINN_KUEUE_ARMED')!r}"
)
assert values["DJINN_KUEUE_LOCAL_QUEUE_PREFIX"] == "kueue-topology-test-djinn", (
    f"queue prefix must match the rendered LocalQueues, got {values.get('DJINN_KUEUE_LOCAL_QUEUE_PREFIX')!r}"
)
PY

echo "=== the acknowledgement does NOT arm anything on its own ==="
# It is an acknowledgement, not a second arming switch.
render "$WORK/ack-without-armed.yaml" \
    --set kueue.enabled=true --set kueue.armed=false --set kueue.namespaceLabelledExternally=true
assert_topology "$WORK/ack-without-armed.yaml" 12 48Gi 3 no

echo "=== the acknowledgement is inert when the chart owns the namespace ==="
# namespace.create=true is unaffected by the new value in either direction.
render "$WORK/ack-with-owned-ns.yaml" \
    --set kueue.enabled=true --set kueue.armed=true --set kueue.namespaceLabelledExternally=true
assert_topology "$WORK/ack-with-owned-ns.yaml" 12 48Gi 3 yes

echo "=== the preflight's namespace RBAC is granted ==="
# The render-time acknowledgement is an unverified claim; djinn-server re-checks
# it at startup by GETting its own Namespace. That read needs a namespaced `get`
# on `namespaces` — without it the preflight can only report "unverifiable" and
# disarms, silently costing the deployment its Kueue quota.
python3 - "$WORK/armed.yaml" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
rules = [
    rule
    for doc in docs
    if doc.get("kind") == "Role"
    for rule in doc.get("rules", [])
    if rule.get("apiGroups") == [""] and "namespaces" in rule.get("resources", [])
]
assert len(rules) == 1, f"expected exactly one namespaces RBAC rule, got {rules}"
# Read-only, and namespaced: the apiserver scopes `get namespaces` in a Role to
# the Role's own namespace, so this grants reading THIS namespace and nothing
# else. Any write verb, or a ClusterRole, would be a real privilege escalation.
assert rules[0].get("verbs") == ["get"], (
    f"the preflight's namespace read must stay get-only, got {rules[0].get('verbs')}"
)
assert not [
    doc for doc in docs
    if doc.get("kind") == "ClusterRole"
    for rule in doc.get("rules", [])
    if "namespaces" in rule.get("resources", [])
], "reading the namespace must NOT require a ClusterRole"
PY

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid kueue.buildPods: $name ==="
    if render_enabled "$WORK/$name.out" "$@" 2>&1; then
        echo "FAIL: invalid Kueue scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

expect_rejected zero --set kueue.buildPods=0
expect_rejected fractional --set kueue.buildPods=1.5
expect_rejected string --set-string kueue.buildPods=seven

echo "=== invalid kueue.enabled ==="
if render "$WORK/enabled-string.out" --set-string kueue.enabled=yes 2>&1; then
    echo "FAIL: a non-boolean kueue.enabled rendered successfully" >&2
    exit 1
fi

echo "=== invalid kueue.armed ==="
if render "$WORK/armed-string.out" --set-string kueue.armed=yes 2>&1; then
    echo "FAIL: a non-boolean kueue.armed rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.enabled ==="
cp -R "$CHART_DIR" "$WORK/chart-without-enabled"
python3 - "$WORK/chart-without-enabled/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values["kueue"].pop("enabled")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-enabled" --is-upgrade >"$WORK/missing-enabled.out" 2>&1; then
    echo "FAIL: missing kueue.enabled rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.armed ==="
cp -R "$CHART_DIR" "$WORK/chart-without-armed"
python3 - "$WORK/chart-without-armed/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values["kueue"].pop("armed")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-armed" --is-upgrade >"$WORK/missing-armed.out" 2>&1; then
    echo "FAIL: missing kueue.armed rendered successfully" >&2
    exit 1
fi

echo "=== invalid kueue.namespaceLabelledExternally ==="
if render "$WORK/ack-string.out" --set-string kueue.namespaceLabelledExternally=yes 2>&1; then
    echo "FAIL: a non-boolean kueue.namespaceLabelledExternally rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.namespaceLabelledExternally ==="
cp -R "$CHART_DIR" "$WORK/chart-without-ack"
python3 - "$WORK/chart-without-ack/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values["kueue"].pop("namespaceLabelledExternally")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-ack" --is-upgrade >"$WORK/missing-ack.out" 2>&1; then
    echo "FAIL: missing kueue.namespaceLabelledExternally rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.buildPods ==="
cp -R "$CHART_DIR" "$WORK/chart-without-kueue"
python3 - "$WORK/chart-without-kueue/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values.pop("kueue")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-kueue" --is-upgrade >"$WORK/missing.out" 2>&1; then
    echo "FAIL: missing kueue.buildPods rendered successfully" >&2
    exit 1
fi

echo "=== kueue ships enabled AND armed, and buildPods unchanged ==="
python3 - "$CHART_DIR/values.yaml" <<'PY'
import sys
import yaml

values = yaml.safe_load(open(sys.argv[1], encoding="utf-8"))
# The stock profile is the production profile: everything armed on the
# production VPS is the chart default. Kueue's pods quota is the ONLY remaining
# build-concurrency bound, so a chart that shipped the topology disarmed would
# ship no bound at all.
assert values["kueue"]["enabled"] is True, (
    f"kueue.enabled must ship true, got {values['kueue']['enabled']!r}"
)
assert values["kueue"]["armed"] is True, (
    f"kueue.armed must ship true, got {values['kueue']['armed']!r}. `enabled` "
    "alone renders queues nothing is admitted into; only `armed` makes the "
    "renderers hand their Jobs to Kueue, so the quota binds nothing without it."
)
# The escape hatch stays opt-in. Shipping it true would pre-acknowledge a claim
# no operator ever made — that an externally-managed namespace already carries
# djinn.io/kueue-managed — which is exactly the silence that gate replaced.
assert values["kueue"]["namespaceLabelledExternally"] is False, (
    "kueue.namespaceLabelledExternally must ship false, got "
    f"{values['kueue']['namespaceLabelledExternally']!r}"
)
PY

# ---------------------------------------------------------------------------
# The zero-capture gate must actually discriminate.
# ---------------------------------------------------------------------------
# deploy/kueue/zero-capture-gate.sh is a live-cluster gate. Run the REAL script
# here with a fake kubectl whose namespace answer is derived from a REAL render
# of this chart, so "the gate passes" and "the gate fails" are decided by the
# chart, not by a hand-written fixture. Without this, arming could silently
# change the Namespace label while the gate kept reporting inert.
GATE="$REPO_ROOT/deploy/kueue/zero-capture-gate.sh"
if [ ! -f "$GATE" ]; then
    echo "FAIL: zero-capture gate is missing: $GATE" >&2
    exit 1
fi

GATE_BIN="$WORK/gate-bin"
mkdir -p "$GATE_BIN"
cat >"$GATE_BIN/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
args=" $* "
if [[ "$args" == *' get namespace djinn '* ]]; then
    cat "$FAKE_NAMESPACE_LABEL"
elif [[ "$args" == *' get pods '* ]]; then
    printf 'Running'
fi
EOF
cat >"$GATE_BIN/helm" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$GATE_BIN/kubectl" "$GATE_BIN/helm"

run_gate_against_render() {
    local name=$1 expectation=$2
    shift 2
    local dir="$WORK/gate-$name"
    mkdir -p "$dir"
    helm template djinn-inert-kueue-gate "$CHART_DIR" --is-upgrade "$@" >"$dir/render.yaml"
    python3 - "$dir/render.yaml" >"$dir/namespace-label.txt" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
namespaces = [doc for doc in docs if doc.get("kind") == "Namespace"]
assert len(namespaces) == 1, f"expected one Namespace, got {len(namespaces)}"
label = namespaces[0].get("metadata", {}).get("labels", {}).get("djinn.io/kueue-managed", "")
sys.stdout.write(str(label))
PY
    set +e
    FAKE_NAMESPACE_LABEL="$dir/namespace-label.txt" \
    KUBECTL="$GATE_BIN/kubectl" \
    HELM="$GATE_BIN/helm" \
    KUEUE_GATE_POLL_SECONDS=0 \
        bash "$GATE" \
            --context fake-disposable \
            --designated-operator-secret fake-designated-operator \
            --timeout-seconds 1 >"$dir/out" 2>&1
    local status=$?
    set -e
    if [ "$expectation" = pass ] && [ "$status" -ne 0 ]; then
        echo "FAIL: zero-capture gate rejected the $name render" >&2
        cat "$dir/out" >&2
        exit 1
    fi
    if [ "$expectation" = fail ] && [ "$status" -eq 0 ]; then
        echo "FAIL: zero-capture gate accepted the $name render; it does not detect the armed state" >&2
        cat "$dir/out" >&2
        exit 1
    fi
}

echo "=== zero-capture gate passes against the inert render the gate itself installs ==="
# The gate owns `kueue.*` and pins BOTH halves on its own install line (see
# deploy/kueue/zero-capture-gate.sh): topology on, arming off. Since `kueue.armed`
# now ships TRUE, that pin is what keeps the inert state representable at all —
# so this case reproduces the gate's own arguments rather than the bare chart
# defaults, which are armed.
run_gate_against_render inert pass --set kueue.enabled=true --set kueue.armed=false

echo "=== zero-capture gate FAILS against an armed render ==="
run_gate_against_render armed fail --set kueue.enabled=true --set kueue.armed=true
grep -q 'djinn.io/kueue-managed=true' "$WORK/gate-armed/out" || {
    echo "FAIL: the armed gate run failed for some other reason than capture:" >&2
    cat "$WORK/gate-armed/out" >&2
    exit 1
}

echo "=== All Kueue topology Helm render tests passed ==="
