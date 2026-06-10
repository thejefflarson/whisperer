use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A request to replicate one Secret from this namespace into others.
///
/// A `Whisper` lives in the **source** namespace and names a Secret in that same
/// namespace. Keeping the source local is the authorization boundary: you can
/// only whisper a secret out of a namespace where you can already create
/// objects, so nobody can replicate a secret they don't own. Targets still have
/// to consent with `whisperer.jeffl.es/allow-sync=true`.
///
/// Replacing the old label-on-Secret + annotation config with a CRD is what lets
/// the operator watch *Whispers* (its own API group) instead of every Secret in
/// the cluster — so it no longer needs cluster-wide secret `list`/`watch`.
#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "whisperer.jeffl.es",
    version = "v1",
    kind = "Whisper",
    namespaced,
    status = "WhisperStatus",
    shortname = "whisp",
    printcolumn = r#"{"name":"Secret","type":"string","jsonPath":".spec.secretName"}"#,
    printcolumn = r#"{"name":"Targets","type":"string","jsonPath":".spec.namespaces"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WhisperSpec {
    /// Name of the Secret in this (the Whisper's own) namespace to replicate.
    pub secret_name: String,
    /// Namespaces to replicate the secret into. Each must opt in with
    /// `whisperer.jeffl.es/allow-sync=true`; protected and unknown namespaces
    /// are skipped (and logged). Bounded so a single Whisper can't inflate
    /// per-reconcile work without limit (the API server rejects oversized specs).
    #[schemars(length(max = 256))]
    pub namespaces: Vec<String>,
}

/// Operator-maintained record of where copies currently live. Orphans are
/// reclaimed by diffing this against the current spec, so cleanup touches only
/// known namespaces — no cluster-wide secret `list` required.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhisperStatus {
    /// Namespaces the secret is currently synced into.
    #[serde(default)]
    pub synced_namespaces: Vec<String>,
}
