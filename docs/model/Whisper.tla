-------------------------------- MODULE Whisper --------------------------------
(***************************************************************************)
(* A formal model of whisperer's per-Whisper reconcile loop.               *)
(*                                                                          *)
(* A `Whisper` declares that a source secret be replicated into a set of    *)
(* target namespaces. The operator writes a copy into each consenting        *)
(* target and reclaims copies that are no longer targets. Where copies live  *)
(* is tracked in `status.syncedNamespaces` and, crucially, rediscoverable by  *)
(* name-probing the namespaces the operator can write to — so there is no      *)
(* cluster-wide secret `list` (the point of the CRD redesign; see ADR 0003).   *)
(*                                                                            *)
(* This models the set algebra in `src/controller.rs` (`discover_copies`,     *)
(* `plan`, `apply`, `cleanup`), abstracting away secret reads, server-side    *)
(* apply, events, and leader election. Copies are tracked by namespace only —  *)
(* a copy is named after its (immutable) Whisper, so its name never enters the  *)
(* reconcile algebra. Two sets of namespaces matter:                          *)
(*                                                                            *)
(*   active — writeNamespaces: where the operator may WRITE copies.           *)
(*   drain  — drainNamespaces: decommissioning, reclaim-only (never a target). *)
(*                                                                            *)
(* The operator can `get`/`delete` in both, so the probe set is active ∪ drain.*)
(*                                                                            *)
(* Checked properties (all hold, including under status loss and             *)
(* decommissioning):                                                          *)
(*   Safety         — a copy only ever exists in a consenting namespace.      *)
(*   Disjoint       — active and drain never overlap.                         *)
(*   CopiesBounded  — every copy stays in active ∪ drain, so it's never        *)
(*                    stranded (always reclaimable by the probe).             *)
(*   CleanupReclaims— deleting a Whisper reclaims all its copies.             *)
(*   Convergence    — reconcile drives copies to exactly the targets.         *)
(***************************************************************************)
EXTENDS FiniteSets

CONSTANTS
    NS,         \* the universe of namespace names
    Consenting  \* namespaces that consent (allow-sync=true) and aren't protected

ASSUME Consenting \subseteq NS

VARIABLES
    wanted,  \* spec.namespaces — the requested target namespaces
    copies,  \* namespaces that currently hold a copy
    status,  \* status.syncedNamespaces — the operator's record of copy locations
    live,    \* whether the Whisper resource still exists
    active,  \* writeNamespaces — namespaces the operator may write copies into
    drain    \* drainNamespaces — decommissioning: reclaim-only, never a target

vars == <<wanted, copies, status, live, active, drain>>

\* Everywhere the operator can still get/delete a copy.
Probe == active \cup drain

\* Eligible targets: requested, consenting, and in the active write set. Draining
\* namespaces are excluded, so their copies fall out of targets and get reclaimed.
Targets == wanted \cap Consenting \cap active

\* discover_copies: the copies the name-probe finds (everything in the probe set).
scanned == copies \cap Probe

TypeOK ==
    /\ wanted \subseteq NS
    /\ copies \subseteq NS
    /\ status \subseteq NS
    /\ live \in BOOLEAN
    /\ active \subseteq NS
    /\ drain \subseteq NS

Init ==
    /\ wanted \in SUBSET NS
    /\ copies = {}
    /\ status = {}
    /\ live = TRUE
    \* writeNamespaces (RBAC: where we CAN write) is independent of Consenting
    \* (the allow-sync label: where we're ALLOWED to write). Starting active = NS
    \* models an operator with write RBAC everywhere, so consent — not RBAC — is
    \* the thing that must keep copies out of non-consenting namespaces. With a
    \* non-consenting namespace in NS \ Consenting, `Safety` is a real check.
    /\ active = NS
    /\ drain = {}

\* Reconcile: the operator's view is status ∪ the probe. Write a copy into every
\* target; reclaim any known copy that is no longer a target; record the result.
Reconcile ==
    /\ live
    /\ LET known    == status \cup scanned
           toDelete == known \ Targets
       IN /\ copies' = (copies \ toDelete) \cup Targets
          /\ status' = Targets
    /\ UNCHANGED <<wanted, live, active, drain>>

\* A user changes the requested target list.
EditSpec ==
    /\ live
    /\ \E w \in SUBSET NS : wanted' = w
    /\ UNCHANGED <<copies, status, live, active, drain>>

\* The status subresource is wiped while copies remain (recreated from a backup,
\* etc.). Survivable: the next reconcile rediscovers the copies via the probe.
StatusLoss ==
    /\ live
    /\ status' = {}
    /\ UNCHANGED <<wanted, copies, live, active, drain>>

\* Decommission: move a namespace from active to drain. It stays in the probe set
\* (still reclaimable) but leaves Targets (so its copy is reclaimed). The safe way
\* to stop syncing into a namespace.
Decommission ==
    /\ live
    /\ \E n \in active :
        /\ active' = active \ {n}
        /\ drain' = drain \cup {n}
    /\ UNCHANGED <<wanted, copies, status, live>>

\* Retire: drop a drained namespace from the probe set — but only once it holds no
\* copy (the admin checking it's empty before deleting it from drainNamespaces).
Retire ==
    /\ live
    /\ \E n \in drain :
        /\ n \notin copies
        /\ drain' = drain \ {n}
    /\ UNCHANGED <<wanted, copies, status, live, active>>

\* Finalizer cleanup: reclaim every copy the operator knows about (status, current
\* targets, and the probe), leaving the source secret alone.
Delete ==
    /\ live
    /\ copies' = copies \ (status \cup Targets \cup scanned)
    /\ status' = {}
    /\ live' = FALSE
    /\ UNCHANGED <<wanted, active, drain>>

Next ==
    Reconcile \/ EditSpec \/ StatusLoss \/ Decommission \/ Retire \/ Delete

Spec == Init /\ [][Next]_vars /\ WF_vars(Reconcile)

\* Sync-only spec (no Delete) for the convergence check under weak fairness.
SyncSpec ==
    Init
    /\ [][Reconcile \/ EditSpec \/ StatusLoss \/ Decommission \/ Retire]_vars
    /\ WF_vars(Reconcile)

(***************************************************************************)
(* Properties.                                                              *)
(***************************************************************************)

Safety == copies \subseteq Consenting
Disjoint == active \cap drain = {}
CopiesBounded == copies \subseteq Probe
CleanupReclaims == (~live) => (copies = {})
Convergence == []<>(copies = Targets)

================================================================================
