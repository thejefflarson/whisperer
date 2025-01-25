# whisperer.rs
A mostly dumb way to make sure every k8s namespace has your pull secrets.

## Ok, what is all this?
**whisperer.rs** is a kubernetes operator that mirrors a secret across namespaces.

It's 300 lines of code, most of which is handling edgecases, I'd love to know if there are more I haven't thought of.

## Cool so how do I use it?
Whelp, right now, maybe hold off? I'm a few days away from CICD and a helm chart, but when that's ready, here's how it works.

Create some namespaces and a secret in a file called `sync.yaml`:
```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: source
---
apiVersion: v1
kind: Namespace
metadata:
  name: target
---
apiVersion: v1
kind: Namespace
metadata:
  name: target2
---
apiVersion: v1
kind: Secret
metadata:
  name: sync
  namespace: source
  labels:
    secret-syncer.jeffl.es/sync: "true"
  annotations:
    secret-syncer.jeffl.es/namespaces: "target,target2,missing"
data:
  secret: dG90YWxseSBhIHN1cGVyIGR1cGVyIHNlY3JldCB5b3Ugc2hvdWxkbid0IHRlbGwgYW55b25lCg==
```

Boot up a k3ds cluster:
```
$ k3d cluster create test
```

Start the operator:
```
$ cargo run
```

And run:
`kubectl apply -f sync.yaml`

Hot diggedy dog, your secrets are synced accross namespaces! 

Just Look at the `target` and `target2` namespaces! You have secrets that mirror the secret you created:
```
$ kubectl get secrets --all-namespaces
NAMESPACE     NAME                                  TYPE                DATA   AGE
kube-system   k3d-test-server-0.node-password.k3s   Opaque              1      23m
kube-system   k3s-serving                           kubernetes.io/tls   2      23m
source        sync                                  Opaque              1      70s
target        sync                                  Opaque              1      26s
target2       sync                                  Opaque              1      26s
```

NEAT-O! No more running `kubectl create secret github ...` in all your namespaces!

## Whelp, that's mildly interesting, how does it work?

I'm glad you asked, the trick is in the labels and annotations on the secret itself:
```yaml
  labels:
    secret-syncer.jeffl.es/sync: "true"
  annotations:
    secret-syncer.jeffl.es/namespaces: "target,target2,missing"
```

The label `secret-syncer.jeffl.es/sync` tells the operator to sync the secret to the comma separated namespaces in the annotation `secret-syncer.jeffl.es/namespaces`.

There are a couple other niceties as well. You can delete a synced secret and it'll come right back:
```
$ kubectl delete secret -n target sync
secret "sync" deleted
$ kubectl get secrets --all-namespaces
NAMESPACE     NAME                                  TYPE                DATA   AGE
kube-system   k3d-test-server-0.node-password.k3s   Opaque              1      29m
kube-system   k3s-serving                           kubernetes.io/tls   2      30m
source        sync                                  Opaque              1      7m27s
target        sync                                  Opaque              1      26s
target2       sync                                  Opaque              1      6m43s
```

You can also search for all your sources of synced secrets:
```
$ kubectl get secrets -l "secret-syncer.jeffl.es/sync=true" --all-namespaces
NAMESPACE   NAME   TYPE     DATA   AGE
source      sync   Opaque   1      10m
```

Or search for "whispered" secrets (haha, great pun jeff):
```
$ kubectl get secrets -l "secret-syncer.jeffl.es/mirror=true" --all-namespaces
NAMESPACE   NAME   TYPE     DATA   AGE
target      sync   Opaque   1      3m57s
target2     sync   Opaque   1      10m
```

Or look for where a particular secret is mirrored to:
```
$ kubectl get secrets -l "secret-syncer.jeffl.es/name=sync,secret-syncer.jeffl.es/namespace=source" --all-namespaces
NAMESPACE   NAME   TYPE     DATA   AGE
target      sync   Opaque   1      5m52s
target2     sync   Opaque   1      12m
```

NEAT-O KEEN-O!

## Yeah, what's the monitoring sitch like?

Well, it has [tracing](https://docs.rs/tracing/latest/tracing/) so all the environment switches work there, and bonus: it also strives to have [opentelemetry](https://opentelemetry.io/) bells and whistles, but honestly I only understand like 5% of that stuff.

## Ok, but like namespaces are supposed to be independent.

Yeah I know.

## Thanks, I hate it, but I'll use it.

Cool! I really like you too!  But if it isn't your thing, delete the source secret and everything is gone:
```
$ kubectl delete secret -n source sync
secret "sync" deleted
$ kubectl get secrets --all-namespaces
NAMESPACE     NAME                                  TYPE                DATA   AGE
kube-system   k3d-test-server-0.node-password.k3s   Opaque              1      43m
kube-system   k3s-serving                           kubernetes.io/tls   2      43m
```
Have a wonderful day!

## One more thing: I feel like other folks have made this.

Sure, but this is how I wanted it to work. Maybe you do too?
