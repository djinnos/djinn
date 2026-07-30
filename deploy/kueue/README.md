# Kueue prerequisite installation

This directory vendors the Kueue installation consumed by Djinn's prerequisite
release. It is deliberately **install and scope only**: this asset does not
label the `djinn` namespace and does not change any Djinn workload, task-run,
warm job, or control-plane object.

## Provenance

| Field | Value |
| --- | --- |
| Upstream project | `kubernetes-sigs/kueue` |
| Release | [`v0.10.0`](https://github.com/kubernetes-sigs/kueue/releases/tag/v0.10.0) |
| Immutable source commit | [`5ff057f44cca5e3ba68e69a20da1bad6cc4974a2`](https://github.com/kubernetes-sigs/kueue/commit/5ff057f44cca5e3ba68e69a20da1bad6cc4974a2) |
| Downloaded upstream asset | `https://github.com/kubernetes-sigs/kueue/releases/download/v0.10.0/manifests.yaml` |
| Downloaded upstream SHA-256 | `f08fb36c8150999af3a73a6b232af598f7e2d9b48082c5051905e4561cadfb5d` |
| Repository-owned manifest | `vendor/kueue-v0.10.0.yaml` |
| Repository-owned manifest SHA-256 | `2c5f4842a21b5423c0f1a172ee8e790f821be67ecc2c555f271521735f4ae954` |

The vendored payload is the upstream release asset byte-for-byte except for
the four webhook entries whose `CREATE` rules cover core `Pod` or batch `Job`
objects: `mpod.kb.io`, `mjob.kb.io`, `vpod.kb.io`, and `vjob.kb.io`.

For each of those entries, the upstream namespace exclusion selector (where
present) is replaced, rather than combined, with these required positive
selectors:

```yaml
namespaceSelector:
  matchLabels:
    djinn.io/kueue-managed: "true"
objectSelector:
  matchLabels:
    djinn.io/kueue-build-object: "true"
```

A namespace must be explicitly labelled before these webhooks can select it,
and an object in that namespace must separately be marked as a build object.
No namespace is labelled by this asset.

## Contract

Run the structural contract with:

```sh
bash deploy/kueue/tests/webhook-selectors.sh
```

The checker parses webhook configuration YAML into mappings and sequences,
then enumerates every `MutatingWebhookConfiguration` and
`ValidatingWebhookConfiguration` rule that covers `CREATE` for `jobs`, `pods`,
their subresources, or a wildcard. It requires both positive `matchLabels`
selectors on the owning webhook. The test passes the vendored manifest and
proves both negative fixture copies are rejected.

## Zero-capture prerequisite gate

The structural contract above proves the selectors are *shaped* correctly. It
does not prove that installing this asset alongside the inert chart captures
nothing. `zero-capture-gate.sh` is the operator-facing harness that proves that
on a real disposable cluster, and
`deploy/runbooks/kueue-inert-release-zero-capture.md` makes a passing
invocation a mandatory prerequisite for the Kueue cutover epic **4c9q**.

Its hermetic fake-`kubectl` contract needs no cluster credentials and runs in
the same globbed roster as the selector test:

```sh
deploy/kueue/tests/zero-capture-gate.sh
```
