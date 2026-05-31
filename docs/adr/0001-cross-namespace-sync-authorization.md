# 1. Cross-namespace sync authorization

Date: 2026-05-30

Status: Accepted

## Context

whisperer copies a source Secret into other namespaces. A source Secret opts in
with the label `whisperer.jeffl.es/sync=true` and lists target namespaces in the
annotation `whisperer.jeffl.es/namespaces`. The operator runs with cluster-wide
RBAC to read, write, and delete Secrets in every namespace.

A security review (see [ADR 0002](0002-security-review-remediation.md)) found
that trusting the source Secret's annotation alone to decide *where* to write was
a Critical cross-tenant injection flaw: any user who can create a Secret in any
one namespace could name an arbitrary target (e.g. `kube-system` or another
tenant) and have the operator plant attacker-controlled data there using its
cluster-wide privileges. The same review found the deletion logic trusted
attacker-spoofable provenance labels (`whisperer.jeffl.es/whisper|name|namespace`)
to decide *what* to delete, giving a cross-namespace data-destruction primitive.

A `SubjectAccessReview` against the source Secret's creator was considered but
rejected: the reconcile loop cannot reliably determine the creator's identity
(managedFields / created-by annotations are absent or spoofable).

## Decision

**Writes — target namespaces must consent.** A namespace is only an eligible
sync target if it carries `whisperer.jeffl.es/allow-sync=true` *and* is not
protected. Consent is granted by whoever administers the target namespace, not by
the author of the source Secret. Implemented as `is_consenting_namespace` /
`resolve_targets` in `src/controller.rs`; requested-but-non-consenting namespaces
are logged as "refused" and skipped.

**Protected namespaces are never targets.** `kube-system`, `kube-public`,
`kube-node-lease`, and the operator's own namespace (`CONTROLLER_NAMESPACE` env
var) are refused even if labelled. See `PROTECTED_NAMESPACES` /
`is_protected_namespace`.

**Deletes — verify operator origin, not labels.** Every cross-namespace delete
goes through `is_managed_copy`, which requires both the `whisper=true` marker
*and* that the Secret was last applied by our server-side-apply field manager
(`whisperer.jeffl.es`). `managedFields` is maintained by the API server and
cannot be forged by a tenant, so a planted look-alike Secret cannot trick the
operator into deleting a victim's data.

We did **not** add a source-namespace allowlist. An earlier plan paired the
target opt-in with a source-namespace restriction, but the target consent label
plus protected-namespace denylist were judged sufficient and simpler; revisit if
a need to restrict which namespaces may *originate* syncs emerges.

## Consequences

- Existing deployments must label each target namespace
  `whisperer.jeffl.es/allow-sync=true` or their copies stop being updated. This
  is a breaking change, documented in the README and the v0.2.0 release notes.
- The cluster-wide RBAC grant remains (it is required for the operator to
  function); the consent label and origin check are the application-layer
  controls that make that grant safe to hold.
- The pure decision functions (`is_consenting_namespace`, `resolve_targets`,
  `is_protected_namespace`, `is_managed_copy`) are unit-tested in CI.
