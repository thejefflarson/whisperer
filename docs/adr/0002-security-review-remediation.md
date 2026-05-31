# 2. Security review remediation (v0.2.0)

Date: 2026-05-30

Status: Accepted

## Context

A full OWASP/CWE security review of whisperer (run against `main` at `8b07c41`)
produced 11 findings across 26 hotspots: 1 Critical, 4 High, 2 Medium, 4 Low.
Two root causes:

1. The controller trusted user-authored Secret labels/annotations for both where
   it writes and what it deletes, with no authorization gate or tamper-proof
   source binding.
2. The CI/CD pipeline ran untrusted code on persistent self-hosted runners and
   signed images with an unverified `cosign` binary.

The review identified three compound attack chains: (a) cross-tenant injection →
privilege escalation via the then-root operator pod; (b) injection + spoofed-label
deletion → full cross-tenant control of Secrets; (c) fork-PR RCE on the runner →
poisoning of the signed, published image.

## Decision

Remediate before the first hardened release (v0.2.0).

**Controller** (detailed in [ADR 0001](0001-cross-namespace-sync-authorization.md)):
target-namespace consent label, protected-namespace denylist, and origin-verified
deletes. Plus: metric labels no longer carry user-controlled name/namespace
(cardinality-explosion DoS), and `Record::drop` no longer panics on clock skew.

**Image & chart.** Runtime image moved from `rust:latest` (root, large) to
`debian:bookworm-slim` with a fixed non-root UID 65532. The chart sets a hardened
pod `securityContext` (`runAsNonRoot`, `runAsUser: 65532`, `readOnlyRootFilesystem`,
drop `ALL`, seccomp `RuntimeDefault`) and pins `image.tag` to the chart
appVersion instead of the mutable `latest`, preserving cosign digest pinning.

**CI/CD.** The self-hosted `rust` job no longer runs on forked PRs. `cosign` is
installed via the SHA-pinned first-party `sigstore/cosign-installer` instead of an
unverified `curl | chmod`. Image and chart digests are validated before signing.
Dependency auditing uses `rustsec/audit-check` (no `cargo install` from crates.io,
which was unreliable on the runner). `dependabot-auto-merge` uses
`pull_request_target` and documents that `--auto` relies on required
branch-protection checks as the CI gate.

**Accepted as-is (not findings).** Cluster-wide Secret RBAC and the mounted
ServiceAccount token are required for a cross-namespace syncer to function. The
leader-election Lease trust model is inherent to Kubernetes Leases and governed
by RBAC. The `/ruok` health endpoint on `0.0.0.0` is required for probes.

## Consequences

- Shipped as **v0.2.0**. PRs: #107 (cleanup), #108 (controller + chart + CI
  hardening, non-root image), #109 (unit tests for the authz logic), #110
  (telemetry `service.name`).
- Breaking change: target namespaces must opt in (see ADR 0001).
- **Caveat:** the non-root image builds and is signed in CI but had not been run
  on a live cluster as part of the release. Validate `runAsNonRoot` /
  `readOnlyRootFilesystem` behaviour on a real cluster.
- The hardened `securityContext` requires the image to run as a non-root UID;
  the non-root Dockerfile and the chart default are coupled and must stay in sync.
