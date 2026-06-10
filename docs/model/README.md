# Formal model of the whisperer reconcile loop

[`Whisper.tla`](Whisper.tla) is a small TLA+ model of whisperer's per-`Whisper`
sync logic — the set algebra in `src/controller.rs` (`discover_copies`, `plan`,
`apply`, `cleanup`), abstracted away from secret reads, server-side apply, and
leader election. Copies are tracked by namespace only: a copy is named after its
(immutable) `Whisper`, so its name never enters the reconcile algebra.

Two namespace sets drive the model:

- **`active`** (`writeNamespaces`) — where the operator may *write* copies.
- **`drain`** (`drainNamespaces`) — namespaces being decommissioned: reclaim-only,
  never a target. The operator can `get`/`delete` in both, so the probe set is
  `active ∪ drain`.

## What it checks

| Property | Meaning | Where it also lives |
|---|---|---|
| `Safety` | A copy only ever exists in a **consenting** namespace — never one that hasn't opted in. | `is_consenting_namespace` / `resolve_targets`, and the `resolve_targets_invariants` proptest |
| `Disjoint` | `active` and `drain` never overlap. | the Helm `fail` guard + the operator's startup `assert` |
| `CopiesBounded` | Every copy stays in `active ∪ drain` — so it's always reachable by the probe and never stranded. | the per-namespace RoleBindings in `chart/templates/rbac.yaml` |
| `CleanupReclaims` | Deleting a `Whisper` reclaims **all** its copies. | `cleanup` in `controller.rs` |
| `Convergence` | Reconcile drives the live copies to exactly the targets, infinitely often (weak fairness on `Reconcile`). | the requeue loop + `plan` |

All hold under the perturbations the design has to survive, modelled as actions:

- **`StatusLoss`** — `status` is wiped while copies remain. Survivable because
  `Reconcile`'s view is `status ∪ scanned`, and the name-probe (`scanned`)
  rediscovers the copies. `CopiesBounded` is what makes the probe exhaustive.
- **`Decommission` / `Retire`** — the safe two-phase way to stop syncing into a
  namespace: `Decommission` moves it from `active` to `drain` (still probed and
  reclaimable, but no longer a target, so its copy is reclaimed), and `Retire`
  drops it from the probe set only once it holds no copy. This is why shrinking the
  write set never strands a copy.

A `secretName` rename doesn't appear in the model: copies are named after the
Whisper, so a rename only changes which source is read, not any copy's identity —
there is nothing for the namespace algebra to get wrong.

The configs use `NS = {n0, n1, n2}` with `Consenting = {n0, n1}` and start
`active = NS`. The point of `n2` is that it's a namespace the operator *can* write
to (it's in the write set) but that has *not* consented — so `Safety` has a real
target to catch. (Drop the `∩ Consenting` from `Targets` and TLC immediately finds
a copy landing in `n2`.) `active` is deliberately independent of `Consenting`:
write RBAC and the allow-sync opt-in are separate gates, and consent is the one
that must keep copies out.

## What it does NOT model

The reconcile is one **atomic** step here: a single `Reconcile` reads status + the
probe and applies all deletes, writes, and the status patch indivisibly. The real
reconcile is a sequence of `await`ed API calls, and the process can crash between
any two of them. So this spec proves the *set algebra* of a reconcile is correct;
it does **not** model partial completion / crash-consistency, nor interleaving of
external mutations between a reconcile's sub-steps. What backs correctness there is
operational rather than proven here: a single leader plus kube's per-object
serialization means no two reconciles of the same Whisper overlap, and the
reconcile is level-triggered and idempotent (desired state is recomputed from
scratch each pass via status ∪ probe, writes are server-side-apply). A sub-stepped,
crash-and-interleave model would be the way to actually check that.

## Running it

Just needs `java` — the script fetches `tla2tools.jar` on first run and checks
both configs:

```bash
scripts/check-model.sh
```

Or by hand with [`tla2tools.jar`](https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar):

```bash
JAR=/path/to/tla2tools.jar
java -cp $JAR tlc2.TLC -config Whisper.cfg Whisper.tla          # safety invariants
java -cp $JAR tlc2.TLC -config WhisperLiveness.cfg Whisper.tla  # Convergence
```

Both report "No error has been found".
