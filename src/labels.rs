pub(crate) const ACTIVE_LABEL: &str = "whisperer.jeffl.es/sync";
pub(crate) const NAMESPACE_ANNOTATION: &str = "whisperer.jeffl.es/namespaces";
pub(crate) const WHISPER_LABEL: &str = "whisperer.jeffl.es/whisper";
pub(crate) const NAME_LABEL: &str = "whisperer.jeffl.es/name";
pub(crate) const NAMESPACE_LABEL: &str = "whisperer.jeffl.es/namespace";
pub(crate) const FINALIZER: &str = "whisperer.jeffl.es/cleanup";

/// A target namespace must carry this label set to `"true"` before whisperer
/// will sync any secret into it. This is the authorization boundary that stops
/// an arbitrary secret author from copying data into namespaces they don't own.
pub(crate) const ALLOW_SYNC_LABEL: &str = "whisperer.jeffl.es/allow-sync";

/// Server-side-apply field manager used for every secret whisperer writes.
/// Copies carry this manager in their `managedFields`, which (unlike the
/// `whisper`/`name`/`namespace` labels) a tenant cannot forge — so we use it to
/// confirm a secret is genuinely operator-managed before deleting it.
pub(crate) const FIELD_MANAGER: &str = "whisperer.jeffl.es";
