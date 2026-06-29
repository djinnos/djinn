#!/usr/bin/env python3
"""Architectural boundary checker — lightweight, no code-graph warm.

Enforces the forbidden-edge rules in ``server/boundary_rules.toml`` against the
workspace's *declared* dependency layering. It has two rule flavours:

* **crate-level** rules (``from_glob`` reduces to a bare crate-name pattern):
  matched against the inter-crate dependency edges declared in each crate's
  ``Cargo.toml`` (``[dependencies]`` / ``[build-dependencies]``; ``dev`` deps
  are test-only and excluded from production layering).
* **file-level** rules (``from_glob`` keeps a path shape): the matching source
  files are scanned for a ``use``-style reference to the forbidden crate.

This reads manifests and greps source — it does NOT invoke ``cargo``, touch a
database, warm a SCIP graph, or compile the workspace. It runs in well under a
second, which is why it is safe to wire as a hard PR / merge-queue gate.

Usage:
    scripts/check_boundaries.py [--rules server/boundary_rules.toml] [--self-test]

Exit codes:
    0  no violations
    1  one or more violations found (human-readable report on stderr)
    2  operational error (unreadable/invalid rules file, no crates found, ...)
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover - exercised only on old runtimes
    print("Error: python 3.11+ (tomllib) is required to run the boundary checker.", file=sys.stderr)
    sys.exit(2)

REPO_ROOT = Path(__file__).resolve().parent.parent
SERVER_DIR = REPO_ROOT / "server"
CRATES_DIR = SERVER_DIR / "crates"

# Boilerplate phrases that mean a rule's description was never filled in.
_BOILERPLATE = ("todo", "fixme", "placeholder", "tbd", "no description",
                "description here", "insert description")


# ---------------------------------------------------------------------------
# Rules: load + validate
# ---------------------------------------------------------------------------

def load_rules(rules_path: Path) -> list[dict]:
    """Parse + fail-closed-validate the rules file. Exits 2 on any problem."""
    try:
        with rules_path.open("rb") as fh:
            config = tomllib.load(fh)
    except OSError as exc:
        _die(f"cannot read rules file '{rules_path}': {exc}")
    except tomllib.TOMLDecodeError as exc:
        _die(f"cannot parse rules file '{rules_path}': {exc}")

    rules = config.get("rules", [])
    if not rules:
        _die(f"no boundary rules defined in '{rules_path}'.")

    errors: list[str] = []
    for i, rule in enumerate(rules):
        name = str(rule.get("name", "")).strip()
        display = name or "<unnamed>"
        if not name:
            errors.append(f"rule[{i}] '{display}' — name: must be nonblank")
        if not str(rule.get("from_glob", "")).strip():
            errors.append(f"rule[{i}] '{display}' — from_glob: must be nonblank")
        if not str(rule.get("to_glob", "")).strip():
            errors.append(f"rule[{i}] '{display}' — to_glob: must be nonblank")
        desc = str(rule.get("description", "")).strip()
        if not desc:
            errors.append(f"rule[{i}] '{display}' — description: must be present and nonblank")
        elif any(b in desc.lower() for b in _BOILERPLATE):
            errors.append(f"rule[{i}] '{display}' — description: must be meaningful (not boilerplate)")

    if errors:
        _die("boundary rule validation failed:\n  " + "\n  ".join(errors))
    return rules


# ---------------------------------------------------------------------------
# Glob helpers
# ---------------------------------------------------------------------------

def normalise_crate_glob(glob: str) -> str:
    """Strip wildcard path segments so a path-style glob reduces to a crate
    name pattern. ``**/djinn-agent/**`` -> ``djinn-agent``; ``**/djinn-*/**``
    -> ``djinn-*``. Mirrors the original crate-level checker's normalisation."""
    s = glob.strip()
    while s.startswith("**/") or s.startswith("*/"):
        s = s[s.index("/") + 1:]
    if s in ("**", "*"):
        return s
    if s.endswith("/**"):
        s = s[:-3]
    elif s.endswith("/*"):
        s = s[:-2]
    return s


def _expand_braces(glob: str) -> list[str]:
    """Expand a single ``{a,b,c}`` alternation into separate globs."""
    m = re.search(r"\{([^{}]*)\}", glob)
    if not m:
        return [glob]
    out: list[str] = []
    for alt in m.group(1).split(","):
        out.extend(_expand_braces(glob[:m.start()] + alt + glob[m.end():]))
    return out


def glob_to_regex(glob: str) -> re.Pattern:
    """Translate a path glob (``**`` spans ``/``, ``*`` does not) to a regex
    matching a full path. Brace alternation is handled by the caller."""
    out = []
    i = 0
    while i < len(glob):
        c = glob[i]
        if glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif c == "*":
            out.append("[^/]*")
            i += 1
        elif c == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(c))
            i += 1
    return re.compile("^" + "".join(out) + "$", re.DOTALL)


def crate_name_matches(pattern: str, name: str) -> bool:
    """Match a crate name against a (normalised) crate-name glob like
    ``djinn-*`` or ``djinn-core``."""
    regex = re.compile("^" + re.escape(pattern).replace(r"\*", "[^/]*") + "$")
    return regex.match(name) is not None


def is_file_level(rule: dict) -> bool:
    """A rule is file-level when its from_glob keeps a path shape (a ``/`` or a
    file suffix survives normalisation) rather than reducing to a crate name."""
    norm = normalise_crate_glob(rule["from_glob"])
    return "/" in norm or norm.endswith(".rs")


# ---------------------------------------------------------------------------
# Workspace model
# ---------------------------------------------------------------------------

def discover_crate_edges() -> tuple[set[str], list[tuple[str, str, str]]]:
    """Return (member crate names, declared inter-crate edges).

    Each edge is (source_crate, target_crate, witness). ``dev-dependencies``
    are excluded: test-only deps are not a production layering violation.
    """
    manifests = sorted(CRATES_DIR.glob("*/Cargo.toml"))
    if (SERVER_DIR / "Cargo.toml").exists():
        manifests.append(SERVER_DIR / "Cargo.toml")
    if not manifests:
        _die(f"no crate manifests found under '{CRATES_DIR}'.")

    parsed: list[tuple[str, dict]] = []
    members: set[str] = set()
    for manifest in manifests:
        with manifest.open("rb") as fh:
            data = tomllib.load(fh)
        name = data.get("package", {}).get("name")
        if not name:
            continue
        members.add(name)
        parsed.append((name, data))

    edges: list[tuple[str, str, str]] = []
    seen: set[tuple[str, str]] = set()
    for name, data in parsed:
        for section in ("dependencies", "build-dependencies"):
            for dep in data.get(section, {}):
                if dep in members and dep != name and (name, dep) not in seen:
                    seen.add((name, dep))
                    edges.append((name, dep, f"crate `{name}` declares `{dep}` in [{section}]"))
    return members, edges


def iter_source_files() -> list[Path]:
    """All ``.rs`` files under the server crates (repo-relative paths)."""
    return sorted(CRATES_DIR.rglob("*.rs"))


# ---------------------------------------------------------------------------
# Checking
# ---------------------------------------------------------------------------

def check_crate_rule(rule: dict, edges: list[tuple[str, str, str]]) -> list[dict]:
    from_pat = normalise_crate_glob(rule["from_glob"])
    to_pat = normalise_crate_glob(rule["to_glob"])
    out = []
    for src, dst, witness in edges:
        if crate_name_matches(from_pat, src) and crate_name_matches(to_pat, dst):
            out.append({"rule": rule, "from_key": src, "to_key": dst, "witness": witness})
    return out


def check_file_rule(rule: dict, files: list[Path]) -> list[dict]:
    target_crate = normalise_crate_glob(rule["to_glob"])
    import_token = re.compile(r"\b" + re.escape(target_crate.replace("-", "_")) + r"\b")
    matchers = [glob_to_regex(g) for g in _expand_braces(rule["from_glob"])]
    out = []
    for path in files:
        rel = path.relative_to(REPO_ROOT).as_posix()
        if not any(m.match(rel) for m in matchers):
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        if import_token.search(text):
            out.append({"rule": rule, "from_key": rel, "to_key": target_crate,
                        "witness": f"{rel} references `{target_crate.replace('-', '_')}`"})
    return out


def render_report(violations: list[dict]) -> str:
    lines = [f"✗ {len(violations)} boundary violation(s) found:\n"]
    for v in violations:
        rule = v["rule"]
        lines.append(f"  {v['from_key']} → {v['to_key']}")
        lines.append(f"      rule name:   {rule['name']}")
        if rule.get("description"):
            lines.append(f"      description: {rule['description']}")
        lines.append(f"      from_key:    {v['from_key']}")
        lines.append(f"      to_key:      {v['to_key']}")
        lines.append(f"      witness:     {v['witness']}")
        lines.append("")
    return "\n".join(lines)


def _die(msg: str) -> "None":
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(2)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def run(rules_path: Path) -> int:
    rules = load_rules(rules_path)
    members, edges = discover_crate_edges()
    files = iter_source_files()

    violations: list[dict] = []
    crate_rule_count = 0
    for rule in rules:
        if is_file_level(rule):
            violations.extend(check_file_rule(rule, files))
        else:
            crate_rule_count += 1
            violations.extend(check_crate_rule(rule, edges))

    if violations:
        sys.stderr.write(render_report(violations))
        return 1

    print(f"✓ No boundary violations found. "
          f"(checked {len(rules)} rule(s) — {crate_rule_count} crate-level — "
          f"against {len(edges)} declared crate edge(s) across {len(members)} crates)")
    return 0


def _self_test() -> int:
    assert normalise_crate_glob("**/djinn-agent/**") == "djinn-agent"
    assert normalise_crate_glob("**/djinn-*/**") == "djinn-*"
    assert normalise_crate_glob("**/actors/slot/pool/{a,b}.rs") == "actors/slot/pool/{a,b}.rs"
    assert crate_name_matches("djinn-*", "djinn-core")
    assert crate_name_matches("djinn-agent", "djinn-agent")
    assert not crate_name_matches("djinn-agent", "djinn-agentx")
    assert is_file_level({"from_glob": "**/actors/slot/pool/{a}.rs", "to_glob": "x"})
    assert not is_file_level({"from_glob": "**/djinn-agent/**", "to_glob": "x"})
    assert sorted(_expand_braces("p/{a,b,c}.rs")) == ["p/a.rs", "p/b.rs", "p/c.rs"]
    assert glob_to_regex("**/actors/slot/pool/actor.rs").match(
        "server/crates/djinn-agent/src/actors/slot/pool/actor.rs")
    assert not glob_to_regex("**/djinn-agent/**").match("server/crates/djinn-db/src/lib.rs")
    # crate rule detects a forbidden edge
    rule = {"name": "x", "from_glob": "**/djinn-a/**", "to_glob": "**/djinn-b/**", "description": "d"}
    assert check_crate_rule(rule, [("djinn-a", "djinn-b", "w")])
    assert not check_crate_rule(rule, [("djinn-a", "djinn-c", "w")])
    print("self-test: OK")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Lightweight architectural boundary checker.")
    parser.add_argument("--rules", type=Path, default=SERVER_DIR / "boundary_rules.toml",
                        help="Path to boundary_rules.toml (default: server/boundary_rules.toml).")
    parser.add_argument("--self-test", action="store_true", help="Run internal logic tests and exit.")
    args = parser.parse_args()
    if args.self_test:
        return _self_test()
    return run(args.rules)


if __name__ == "__main__":
    sys.exit(main())
