use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::{ResourceExt, api::ObjectMeta};

use crate::labels::*;

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
        // Carry the source's labels/annotations for traceability, but strip any
        // whisperer-managed keys first so a crafted source can't smuggle in a
        // stale marker (e.g. a forged owner-uid). Then stamp our own.
        let strip_managed = |m: &BTreeMap<String, String>| {
            m.iter()
                .filter(|(k, _)| !k.starts_with(MANAGED_PREFIX))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<String, String>>()
        };
        let mut labels = strip_managed(self.labels());
        labels.insert(WHISPER_LABEL.to_string(), "true".to_string());
        labels.insert(OWNER_UID_LABEL.to_string(), owner_uid.to_string());
        labels.insert(NAME_LABEL.to_string(), name.to_string());
        labels.insert(NAMESPACE_LABEL.to_string(), source_ns.to_string());

        let annotations = strip_managed(self.annotations());

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
