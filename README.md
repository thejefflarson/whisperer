# whisperer
A mostly dumb way to make sure every k8s namespace has your pull secrets.

## Ok, what is all this?
**whisperer** is a kubernetes operator that whispers a secret from one namespace into others.

It's about 300 lines of core code in `src/controller.rs`, most of which is handling edgecases, I'd love to know if there are more I haven't thought of.

You tell it what to copy with a little `Whisper` resource — a Custom Resource that
lives next to your secret and says "replicate me into these namespaces." (It used
to be driven by labels and annotations on the Secret itself; that meant the
operator had to read *every Secret in the cluster* to find the labelled ones. The
`Whisper` CRD lets it watch its own resources instead, so it never needs
cluster-wide secret access. See [ADR 0003](docs/adr/0003-crd-scoped-rbac.md) and
the [formal model](docs/model/) if you like that sort of thing.)

## Cool, so how do I use it?

Install it with Helm. This installs the `Whisper` CRD and the operator, and grants
the operator write access to exactly the namespaces you list in `writeNamespaces`:
```
$ helm install whisperer oci://ghcr.io/thejefflarson/charts/whisperer \
    --namespace whisperer --create-namespace \
    --set 'writeNamespaces={target,target2}'
```

**Two ground rules:**

1. **The source secret and its `Whisper` live in the operator's namespace.** That's
   where whisperer reads sources and watches for changes. (Running locally with
   `cargo run`? That's your kubeconfig's current namespace, usually `default`.)
2. **Target namespaces have to opt in** by carrying the label
   `whisperer.jeffl.es/allow-sync=true`. Namespaces without it — and the protected
   `kube-system`, `kube-public`, `kube-node-lease`, and the operator's own
   namespace — are skipped. That's what stops anyone who can create a Secret from
   whispering it into a namespace they don't own.

So label your targets:
```
$ kubectl label namespace target  whisperer.jeffl.es/allow-sync=true
$ kubectl label namespace target2 whisperer.jeffl.es/allow-sync=true
```

Then drop your secret and a `Whisper` into the operator's namespace in a file
called `sync.yaml`:
```yaml
apiVersion: v1
kind: Secret
metadata:
  name: sync
  namespace: whisperer
data:
  secret: dG90YWxseSBhIHN1cGVyIGR1cGVyIHNlY3JldCB5b3Ugc2hvdWxkbid0IHRlbGwgYW55b25lCg==
---
apiVersion: whisperer.jeffl.es/v1
kind: Whisper
metadata:
  name: sync
  namespace: whisperer
spec:
  # The Secret in THIS namespace to replicate.
  secretName: sync
  # Where to send it. Each must consent (allow-sync=true) and be listed in the
  # chart's writeNamespaces. "missing" is fine — whisperer just grumbles and
  # catches up if you create it later.
  namespaces: ["target", "target2", "missing"]
```

And apply it:
```
$ kubectl apply -f sync.yaml
```

Hot diggedy dog, your secret is synced across namespaces!

Just look at the `target` and `target2` namespaces — secrets that mirror the one you created:
```
$ kubectl get secrets --all-namespaces
NAMESPACE     NAME                                  TYPE                DATA   AGE
kube-system   k3d-test-server-0.node-password.k3s   Opaque              1      23m
kube-system   k3s-serving                           kubernetes.io/tls   2      23m
whisperer     sync                                  Opaque              1      70s
target        sync                                  Opaque              1      26s
target2       sync                                  Opaque              1      26s
```

NEAT-O! No more running `kubectl create secret github ...` in all your namespaces!

## Whelp, that's mildly interesting, how does it work?

The trick is the `Whisper` resource. The operator watches `Whisper`s (its own API
group — *not* every Secret in the cluster), and for each one it reads the named
secret from that `Whisper`'s namespace and copies it into every target that has
consented with `whisperer.jeffl.es/allow-sync=true`.

**The copy takes the `Whisper`'s name**, not the source secret's — so name the
`Whisper` what you want the replicated secret to be called (`secretName` just picks
which source to read). Naming copies after the stable `Whisper` is what lets a
later `secretName` change refresh the copy in place instead of leaving a stray
old-named secret behind; each copy also carries the `Whisper`'s UID so the operator
can always find and reclaim it, even if its status is lost.

You can see your whispers and where they've landed:
```
$ kubectl get whispers --all-namespaces
NAMESPACE   NAME   SECRET   TARGETS
whisperer   sync   sync     ["target","target2","missing"]
```

It tracks where copies currently live in the `Whisper`'s status, so it can clean
up after itself without ever listing secrets cluster-wide:
```
$ kubectl get whisper sync -n whisperer -o jsonpath='{.status.syncedNamespaces}'
["target","target2"]
```

The copies carry marker labels so you can find them. Search for "whispered"
secrets (haha, great pun jeff):
```
$ kubectl get secrets -l "whisperer.jeffl.es/whisper=true" --all-namespaces
NAMESPACE   NAME   TYPE     DATA   AGE
target      sync   Opaque   1      3m57s
target2     sync   Opaque   1      10m
```

Or look for where a particular secret is whispered to:
```
$ kubectl get secrets -l "whisperer.jeffl.es/name=sync,whisperer.jeffl.es/namespace=whisperer" --all-namespaces
NAMESPACE   NAME   TYPE     DATA   AGE
target      sync   Opaque   1      5m52s
target2     sync   Opaque   1      12m
```

And the niceties still hold: delete a whispered copy and it comes right back on
the next reconcile. Drop a namespace from `spec.namespaces` and its copy gets
reclaimed. NEAT-O KEEN-O!

## What can it actually touch? (Permissions)

The whole reason for the `Whisper` CRD is to keep the operator on a short leash.
It has **no cluster-wide secret access at all**. Precisely:

| Resource | Verbs | Scope |
|----------|-------|-------|
| `whispers` (+ `/status`, `/finalizers`) | get/list/watch/update/patch | cluster-wide — its own API group, not secrets |
| secrets | get/list/watch | **operator namespace only** (reads sources) |
| secrets | get/create/update/patch/delete | **only the namespaces in `writeNamespaces`** (one RoleBinding each) |
| secrets | get/delete | **only the namespaces in `drainNamespaces`** (one RoleBinding each) |
| namespaces | get/list/watch | cluster-wide (just metadata, for the consent check) |
| events / leases | create/patch · leader election | operator namespace |

`writeNamespaces` must be a superset of every `Whisper`'s `spec.namespaces`; a
target that consents but isn't in the list gets refused by the apiserver, not the
operator.

**Done syncing into a namespace?** Don't just yank it out of `writeNamespaces` —
that strands the copies already there (the operator loses the rights to clean them
up). Move it to `drainNamespaces` instead: the operator stops writing there, keeps
just enough access (`get`+`delete`) to reclaim what's left, and once
`kubectl get secret <whisper> -n <ns>` comes up empty you can drop it entirely. The
two lists must be disjoint (the chart won't render, and the operator won't start,
otherwise). Full rationale in [ADR 0003](docs/adr/0003-crd-scoped-rbac.md).

## Yeah, what's the monitoring sitch like?

Well, it has [tracing](https://docs.rs/tracing/latest/tracing/) so all the environment switches work there, and bonus: it also strives to have [opentelemetry](https://opentelemetry.io/) bells and whistles, and there's a prometheus endpoint if that's your jam, but honestly I only understand like 5% of that stuff.

## Ok, but like namespaces are supposed to be independent.

Yeah I know.

## Thanks, I hate it.

Cool! I really like you too! But if it isn't your thing, delete the `Whisper` and
the copies vanish — your original secret is left right where you put it, because
the `Whisper` was only ever a *declaration* to sync, not the secret itself:
```
$ kubectl delete whisper -n whisperer sync
whisper.whisperer.jeffl.es "sync" deleted
$ kubectl get secrets -l "whisperer.jeffl.es/whisper=true" --all-namespaces
No resources found
$ kubectl get secret sync -n whisperer
NAME   TYPE     DATA   AGE
sync   Opaque   1      14m
```
Have a wonderful day!

## Hacking on it

Run the unit + property tests (no cluster needed):
```
$ cargo nextest run
```
Run the live-cluster integration tests against a throwaway [k3d](https://k3d.io) cluster:
```
$ scripts/integration-test.sh
```
Regenerate the CRD manifest after changing `src/whisper.rs`:
```
$ cargo run --bin crdgen > chart/crds/whisper.yaml
```

## One more thing: I feel like other folks have made this.

Sure, but this is how I wanted it to work. Maybe you do too?
