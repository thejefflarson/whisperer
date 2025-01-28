use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::{api::ObjectMeta, Resource, ResourceExt};

use crate::labels::*;

pub(crate) trait SecretExt {
    fn dup(&self, ns: String) -> Self;
}

impl SecretExt for Secret {
    // Duplicate a secret into namespace with the correct labels in place.
    fn dup(&self, ns: String) -> Secret {
        let meta = self.meta();
        let name = self.name_any();
        let namespace = self.namespace().unwrap_or(String::from(""));
        // I could an argument to see not syncing labels and annotations.
        let mut labels = self
            .labels()
            .clone()
            .iter()
            .filter(|it| it.0 != ACTIVE_LABEL)
            .map(|it| (it.0.to_owned(), it.1.to_owned()))
            .collect::<BTreeMap<String, String>>();
        labels.insert(NAMESPACE_LABEL.to_string(), namespace.clone());
        labels.insert(NAME_LABEL.to_string(), name.clone());
        labels.insert(WHISPER_LABEL.to_string(), "true".to_string());
        let annotations = self
            .annotations()
            .clone()
            .iter()
            .filter(|it| it.0 != NAMESPACE_ANNOTATION)
            .map(|it| (it.0.to_owned(), it.1.to_owned()))
            .collect::<BTreeMap<String, String>>();
        let finalizers = meta
            .finalizers
            .clone()
            .map(|it| it.into_iter().filter(|it| it != FINALIZER).collect());
        Secret {
            data: self.data.clone(),
            immutable: self.immutable,
            metadata: ObjectMeta {
                annotations: Some(annotations),
                deletion_grace_period_seconds: meta.deletion_grace_period_seconds,
                finalizers,
                generate_name: meta.generate_name.clone(),
                labels: Some(labels),
                name: meta.name.clone(),
                namespace: Some(ns.clone()),
                self_link: meta.self_link.clone(),
                ..ObjectMeta::default()
            },
            string_data: self.string_data.clone(),
            type_: self.type_.clone(),
        }
    }
}
