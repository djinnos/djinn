#!/usr/bin/env python3
"""Check positive Kueue selectors on every Job/Pod CREATE admission rule.

The checker intentionally parses the relevant Kubernetes YAML documents into
nested Python mappings/lists. It does not depend on the source ordering of a
webhook's selectors and rules, so an upstream addition cannot escape merely by
moving fields around.
"""

from __future__ import annotations

import argparse
import ast
import json
import re
import sys
from pathlib import Path
from typing import Any

WEBHOOK_KINDS = {"MutatingWebhookConfiguration", "ValidatingWebhookConfiguration"}
NAMESPACE_LABEL = "djinn.io/kueue-managed"
OBJECT_LABEL = "djinn.io/kueue-build-object"


class YamlError(ValueError):
    pass


def scalar(value: str) -> Any:
    """Decode the scalar forms used by Kubernetes installation manifests."""
    value = value.strip()
    if value in {"", "null", "Null", "NULL", "~"}:
        return None
    if value in {"true", "True", "TRUE"}:
        return True
    if value in {"false", "False", "FALSE"}:
        return False
    if value in {"{}", "[]"}:
        return {} if value == "{}" else []
    if value.startswith('"') and value.endswith('"'):
        return json.loads(value)
    if value.startswith("'") and value.endswith("'"):
        return ast.literal_eval(value)
    return value


def tokens(document: str) -> list[tuple[int, str]]:
    """Tokenize indentation while excluding comments and block-scalar bodies."""
    result: list[tuple[int, str]] = []
    block_indent: int | None = None
    for line_no, raw in enumerate(document.splitlines(), 1):
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        if "\t" in raw[:indent]:
            raise YamlError(f"line {line_no}: tabs are not supported")
        if block_indent is not None:
            if indent > block_indent:
                continue
            block_indent = None
        content = raw[indent:]
        if re.search(r":\s*[>|][+-]?\s*(?:#.*)?$", content):
            block_indent = indent
        result.append((indent, content))
    return result


def split_key_value(content: str) -> tuple[str, str | None]:
    if ":" not in content:
        raise YamlError(f"expected mapping entry, got {content!r}")
    key, value = content.split(":", 1)
    if not key:
        raise YamlError(f"empty mapping key in {content!r}")
    return key, value.strip() or None


def parse_document(document: str) -> Any:
    stream = tokens(document)

    def parse_node(index: int, indent: int) -> tuple[Any, int]:
        if index >= len(stream) or stream[index][0] < indent:
            return None, index
        if stream[index][0] != indent:
            raise YamlError(f"unexpected indentation {stream[index][0]}, expected {indent}")
        if stream[index][1].startswith("- ") or stream[index][1] == "-":
            return parse_list(index, indent)
        return parse_map(index, indent)

    def parse_map(index: int, indent: int, initial: tuple[str, str | None] | None = None) -> tuple[dict[str, Any], int]:
        output: dict[str, Any] = {}
        if initial is not None:
            key, value = initial
            if value is None:
                # A list item's first mapping key is written on the same line
                # as the dash (for example, ``- operations:``). Its nested
                # sequence begins at this map's indentation, while ordinary
                # nested mapping values begin deeper.
                if index < len(stream) and (
                    stream[index][0] > indent
                    or (stream[index][0] == indent and stream[index][1].startswith("-"))
                ):
                    output[key], index = parse_node(index, stream[index][0])
                else:
                    output[key] = None
            else:
                output[key] = scalar(value)
        while index < len(stream) and stream[index][0] == indent and not stream[index][1].startswith("- "):
            key, value = split_key_value(stream[index][1])
            index += 1
            if value is None:
                # Kubernetes commonly places a sequence at the same
                # indentation as its mapping key (``webhooks:\n- name:``).
                if index < len(stream) and (
                    stream[index][0] > indent
                    or (stream[index][0] == indent and stream[index][1].startswith("-"))
                ):
                    output[key], index = parse_node(index, stream[index][0])
                else:
                    output[key] = None
            else:
                output[key] = scalar(value)
        return output, index

    def parse_list(index: int, indent: int) -> tuple[list[Any], int]:
        output: list[Any] = []
        while index < len(stream) and stream[index][0] == indent and (stream[index][1].startswith("- ") or stream[index][1] == "-"):
            rest = stream[index][1][1:].strip()
            index += 1
            if not rest:
                if index < len(stream) and stream[index][0] > indent:
                    value, index = parse_node(index, stream[index][0])
                else:
                    value = None
            elif ":" in rest:
                key, inline_value = split_key_value(rest)
                child_indent = stream[index][0] if index < len(stream) and stream[index][0] > indent else indent + 2
                value, index = parse_map(index, child_indent, (key, inline_value))
            else:
                value = scalar(rest)
            output.append(value)
        return output, index

    if not stream:
        return None
    value, index = parse_node(0, stream[0][0])
    if index != len(stream):
        raise YamlError("unparsed YAML content remains")
    return value


def documents(path: Path) -> list[dict[str, Any]]:
    # Kubernetes multi-document separators occur on their own unindented line.
    result: list[dict[str, Any]] = []
    for document in re.split(r"^---\s*(?:#.*)?$", path.read_text(encoding="utf-8"), flags=re.MULTILINE):
        if not re.search(r"^kind:\s*(?:Mutating|Validating)WebhookConfiguration\s*$", document, flags=re.MULTILINE):
            continue
        parsed = parse_document(document)
        if not isinstance(parsed, dict):
            raise YamlError("webhook configuration is not a mapping")
        result.append(parsed)
    return result


def covers_job_or_pod_create(rule: Any) -> bool:
    if not isinstance(rule, dict):
        return False
    operations = rule.get("operations", [])
    resources = rule.get("resources", [])
    if not isinstance(operations, list) or not isinstance(resources, list):
        return False
    creates = any(operation in {"CREATE", "*"} for operation in operations)
    targets = any(
        isinstance(resource, str)
        and (resource == "*" or resource in {"jobs", "pods"} or resource.startswith("jobs/") or resource.startswith("pods/"))
        for resource in resources
    )
    return creates and targets


def required_label(webhook: dict[str, Any], selector_name: str, label: str) -> bool:
    selector = webhook.get(selector_name)
    return (
        isinstance(selector, dict)
        and isinstance(selector.get("matchLabels"), dict)
        and selector["matchLabels"].get(label) == "true"
    )


def check(path: Path) -> list[str]:
    failures: list[str] = []
    relevant = 0
    configurations = documents(path)
    if not configurations:
        return [f"{path}: no admission webhook configurations found"]
    for configuration in configurations:
        kind = configuration.get("kind")
        if kind not in WEBHOOK_KINDS:
            continue
        metadata_value = configuration.get("metadata")
        metadata: dict[str, Any] = metadata_value if isinstance(metadata_value, dict) else {}
        config_name = metadata.get("name", "<unnamed>")
        webhooks = configuration.get("webhooks", [])
        if not isinstance(webhooks, list):
            failures.append(f"{kind}/{config_name}: webhooks is not a list")
            continue
        for webhook in webhooks:
            if not isinstance(webhook, dict):
                continue
            rules = webhook.get("rules", [])
            if not isinstance(rules, list) or not any(covers_job_or_pod_create(rule) for rule in rules):
                continue
            relevant += 1
            name = webhook.get("name", "<unnamed>")
            prefix = f"{kind}/{config_name} webhook {name}"
            if not required_label(webhook, "namespaceSelector", NAMESPACE_LABEL):
                failures.append(f"{prefix}: namespaceSelector.matchLabels[{NAMESPACE_LABEL!r}] must be 'true'")
            if not required_label(webhook, "objectSelector", OBJECT_LABEL):
                failures.append(f"{prefix}: objectSelector.matchLabels[{OBJECT_LABEL!r}] must be 'true'")
    if relevant == 0:
        failures.append(f"{path}: no Job/Pod CREATE webhook rules found")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()
    try:
        failures = check(args.manifest)
    except (OSError, ValueError, json.JSONDecodeError, SyntaxError) as error:
        print(f"FAIL: cannot parse {args.manifest}: {error}", file=sys.stderr)
        return 2
    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        return 1
    print(f"PASS: {args.manifest}: all Job/Pod CREATE webhooks have both positive selectors")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
