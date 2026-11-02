use kube::CustomResource;
use schemars::{JsonSchema, SchemaGenerator, Schema};
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
/// JSON Schema for `spec.namespaces` that enforces both a max-items bound
/// and per-item RFC 1123 DNS label format. Using `schema_with` instead of a
/// field-level `schemars` attribute because schemars cannot add per-item
/// constraints to `Vec<String>` without a wrapper type.
fn namespaces_schema(_: &mut SchemaGenerator) -> Schema {
    // Each namespace name must be a valid RFC 1123 DNS label:
    // lowercase alphanumeric or hyphen, start/end with alphanumeric, ≤ 63 chars.
    // maxItems is kept low (32) so a single Whisper cannot drive O(32 ×
    // |writeNamespaces|) API probes per reconcile and overload the API server.
    Schema::from(serde_json::json!({
        "type": "array",
        "items": {
            "type": "string",
            "pattern": "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$",
            "maxLength": 63
        },
        "maxItems": 32,
        "description": "Namespaces to replicate the secret into. Each must opt in \
            with whisperer.jeffl.es/allow-sync=true; protected and unknown namespaces \
            are skipped. Capped at 32 entries so a single Whisper cannot inflate \
            per-reconcile API work without limit."
    }))
}

#[serde(rename_all = "camelCase")]
pub struct WhisperSpec {
    /// Name of the Secret in this (the Whisper's own) namespace to replicate.
    /// Must be a valid Kubernetes DNS subdomain name (RFC 1123): lowercase
    /// alphanumeric, hyphens, and dots; ≤ 253 chars; start/end alphanumeric.
    /// Validated at the API server by the CRD schema so invalid names are
    /// rejected at admission rather than discovered on every reconcile.
    #[schemars(
        length(max = 253),
        pattern = "^[a-z0-9][a-z0-9\\.\\-]*[a-z0-9]$|^[a-z0-9]$"
    )]
    pub secret_name: String,
    /// Namespaces to replicate the secret into. See [`namespaces_schema`] for
    /// CRD-level per-item format constraints.
    #[schemars(schema_with = "namespaces_schema")]
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
