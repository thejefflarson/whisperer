/// Marks managed synced copies. Primarily a **discoverability** label — it's how a
/// human finds every copy whisperer made (`kubectl get secrets -l
/// whisperer.jeffl.es/whisper=true -A`). It's a public constant, so it is not a
/// security signal; attribution is done by [`OWNER_UID_LABEL`].
pub(crate) const WHISPER_LABEL: &str = "whisperer.jeffl.es/whisper";
/// On a copy, the owning `Whisper`'s name (for humans / `kubectl -l` filtering).
pub(crate) const NAME_LABEL: &str = "whisperer.jeffl.es/name";
/// On a copy, the source (Whisper's) namespace (for humans / `kubectl -l`).
pub(crate) const NAMESPACE_LABEL: &str = "whisperer.jeffl.es/namespace";
pub(crate) const FINALIZER: &str = "whisperer.jeffl.es/cleanup";

/// On a copy, the owning `Whisper`'s UID — how the operator attributes a found
/// copy to the right Whisper (robust across `spec.secretName` renames, since
/// copies are named after the Whisper). This is the strongest copy marker: the UID
/// is server-assigned, immutable, and a random UUID, so unlike the public-constant
/// `whisper` label and field manager, a tenant can't set a *matching* value
/// without first learning the Whisper's UID (reading the Whisper or a copy). The
/// hard authorization boundary is still RBAC + delete preconditions, not labels.
pub(crate) const OWNER_UID_LABEL: &str = "whisperer.jeffl.es/owner-uid";

/// A target namespace must carry this label set to `"true"` before whisperer will
/// sync any secret into it. This is the authorization boundary that stops an
/// arbitrary secret author from copying data into namespaces they don't own.
pub(crate) const ALLOW_SYNC_LABEL: &str = "whisperer.jeffl.es/allow-sync";

/// Server-side-apply field manager used for every secret whisperer writes.
/// Copies carry this manager in their `managedFields`, which (unlike the
/// `whisper`/`name`/`namespace` labels) a tenant cannot forge — so we use it to
/// confirm a secret is genuinely operator-managed before deleting it.
pub(crate) const FIELD_MANAGER: &str = "whisperer.jeffl.es";

/// Prefix for every label/annotation whisperer manages. Copies strip any
/// incoming keys with this prefix from the source before re-stamping their own,
/// so a crafted source secret can't smuggle in a stale marker.
pub(crate) const MANAGED_PREFIX: &str = "whisperer.jeffl.es/";
