//! Emit the `Whisper` CustomResourceDefinition as YAML on stdout.
//!
//! Run `cargo run --bin crdgen > chart/crds/whisper.yaml` to regenerate the
//! manifest the Helm chart installs. Keeping the CRD generated from the Rust
//! type (rather than hand-written) means the schema can never drift from the
//! `WhisperSpec`/`WhisperStatus` the controller actually deserializes.

use kube::CustomResourceExt;
use whisperer::whisper::Whisper;

fn main() {
    let crd = Whisper::crd();
    print!(
        "{}",
        serde_yaml::to_string(&crd).expect("Whisper CRD should serialize to YAML")
    );
}
