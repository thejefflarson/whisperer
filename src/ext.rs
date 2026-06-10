use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::{ResourceExt, api::ObjectMeta};

use crate::labels::*;

/// GitOps/tooling label & annotation prefixes that identify which chart/app owns
/// a resource. If a copy carries these (inherited from the source), Argo CD / Flux
/// adopt the copy as part of the *source's* release — flagging it out-of-sync and,
/// if it shares the owning app's namespace, pruning it out from under the operator.
/// A whispered copy must be owned only by whisperer, so these are stripped.
const TOOLING_PREFIXES: &[&str] = &[
    "app.kubernetes.io/",
    "helm.sh/",
    "argocd.argoproj.io/",
    "kustomize.toolkit.fluxcd.io/",
    "kubectl.kubernetes.io/",
];

pub(crate) trait SecretExt {
    fn dup(&self, name: &str, owner_uid: &str, source_ns: &str, target_ns: String) -> Self;
}

impl SecretExt for Secret {
    /// Duplicate this (source) secret's contents into `target_ns` as a copy named
    /// `name` — the owning Whisper's name, NOT the source secret's name. Naming
    /// the copy after the Whisper keeps its identity stable across
    /// `spec.secretName` renames, so a rename refreshes the copy in place instead
    /// of orphaning the old-named one. `owner_uid` and `source_ns` are stamped as
    /// labels so the operator can attribute the copy back to its Whisper.
    fn dup(&self, name: &str, owner_uid: &str, source_ns: &str, target_ns: String) -> Secret {
        // Carry the source's labels/annotations for traceability, but strip
        // whisperer-managed keys (so a crafted source can't smuggle a stale marker
        // like a forged owner-uid) AND GitOps/tooling ownership keys (so Argo/Flux
        // don't adopt the copy as part of the source's release). Then stamp our own.
        let strip = |m: &BTreeMap<String, String>| {
            m.iter()
                .filter(|(k, _)| {
                    !k.starts_with(MANAGED_PREFIX)
                        && !TOOLING_PREFIXES.iter().any(|p| k.starts_with(p))
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<String, String>>()
        };
        let mut labels = strip(self.labels());
        labels.insert(WHISPER_LABEL.to_string(), "true".to_string());
        labels.insert(OWNER_UID_LABEL.to_string(), owner_uid.to_string());
        labels.insert(NAME_LABEL.to_string(), name.to_string());
        labels.insert(NAMESPACE_LABEL.to_string(), source_ns.to_string());

        let annotations = strip(self.annotations());

        Secret {
            data: self.data.clone(),
            immutable: self.immutable,
            metadata: ObjectMeta {
                annotations: Some(annotations),
                labels: Some(labels),
                name: Some(name.to_string()),
                namespace: Some(target_ns),
                ..ObjectMeta::default()
            },
            string_data: self.string_data.clone(),
            type_: self.type_.clone(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use kube::api::ObjectMeta;

    fn secret_with(labels: &[(&str, &str)]) -> Secret {
        Secret {
            metadata: ObjectMeta {
                name: Some("github".into()),
                namespace: Some("whisperer".into()),
                labels: Some(
                    labels
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                ..ObjectMeta::default()
            },
            ..Secret::default()
        }
    }

    #[test]
    fn dup_strips_tooling_and_managed_labels_but_keeps_others() {
        let copy = secret_with(&[
            ("app.kubernetes.io/instance", "whisperer"),
            ("helm.sh/chart", "whisperer-0.3.0"),
            ("argocd.argoproj.io/instance", "whisperer"),
            ("whisperer.jeffl.es/whisper", "true"),
            ("team", "platform"),
        ])
        .dup("github", "uid-123", "whisperer", "analytics".to_string());
        let labels = copy.metadata.labels.unwrap();

        // GitOps/tooling ownership labels must NOT propagate to the copy, or Argo
        // CD / Flux adopt it as part of the source's release.
        assert!(!labels.contains_key("app.kubernetes.io/instance"));
        assert!(!labels.contains_key("helm.sh/chart"));
        assert!(!labels.contains_key("argocd.argoproj.io/instance"));
        // A plain source label is still carried through for traceability.
        assert_eq!(labels.get("team").map(String::as_str), Some("platform"));
        // whisperer's own markers are (re)stamped.
        assert_eq!(labels.get(WHISPER_LABEL).map(String::as_str), Some("true"));
        assert_eq!(
            labels.get(OWNER_UID_LABEL).map(String::as_str),
            Some("uid-123")
        );
        assert_eq!(labels.get(NAME_LABEL).map(String::as_str), Some("github"));
        assert_eq!(
            labels.get(NAMESPACE_LABEL).map(String::as_str),
            Some("whisperer")
        );
        assert_eq!(copy.metadata.namespace.as_deref(), Some("analytics"));
    }
}
