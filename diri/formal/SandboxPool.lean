import Std

/-!
# Ghost Desktop pool

A model of `diri-engine/src/sandbox.rs`. The pool holds live slots and
in-flight launches; each slot is leased to the session that started it or
to the person. The backend's processes and private directories are
`resources`, keyed by sandbox id, and are only ever released by reclaiming
the slot that owns them.

Proved here:
* `Inv` holds initially and after every operation, so live slots plus
  in-flight launches never exceed `capacity` and ids are never reused;
* `lease_exclusive`: two sessions are never both authorized for one sandbox;
* `release_leaks_nothing`: after a session ends, none of its sandboxes
  remain and the resources held are exactly those of the remaining slots;
* `reclaim_leaks_nothing`: the same for every removal (stop, idle, backend
  death), and `engine_crash_leaks_nothing` for an Engine killed outright.
-/

namespace SandboxPool

abbrev Session := Nat
abbrev SandboxId := Nat

inductive Owner where
  | person
  | session (s : Session)
  deriving DecidableEq

inductive Caller where
  | person
  | session (s : Session)
  deriving DecidableEq

structure Slot where
  id : SandboxId
  owner : Owner
  deriving DecidableEq

structure Pool where
  capacity : Nat
  slots : List Slot
  /-- `Reservation`s: starts whose launch is in flight. -/
  inflight : List SandboxId
  nextId : SandboxId
  /-- What the backend holds: processes and state directories, by id. -/
  resources : List SandboxId

def ids (p : Pool) : List SandboxId := p.slots.map Slot.id

structure Inv (p : Pool) : Prop where
  bounded : p.slots.length + p.inflight.length ≤ p.capacity
  unique : (ids p ++ p.inflight).Nodup
  fresh : ∀ i ∈ ids p ++ p.inflight, i < p.nextId
  /-- No orphan and no unbacked slot: resources are exactly the live slots'. -/
  backed : p.resources = ids p

def empty (capacity : Nat) : Pool := ⟨capacity, [], [], 0, []⟩

theorem empty_inv (capacity : Nat) : Inv (empty capacity) := by
  constructor <;> simp [empty, ids]

/-! ## Operations, mirroring `SandboxPool` -/

/-- `start`: reserve a slot before launching, or refuse with `POOL_FULL`. -/
def reserve (p : Pool) : Option (Pool × SandboxId) :=
  if p.slots.length + p.inflight.length < p.capacity then
    some ({ p with inflight := p.nextId :: p.inflight, nextId := p.nextId + 1 }, p.nextId)
  else none

/-- The launch succeeded: the reservation becomes a slot with its resources. -/
def fill (p : Pool) (i : SandboxId) (o : Owner) : Pool :=
  if i ∈ p.inflight then
    { p with slots := ⟨i, o⟩ :: p.slots, inflight := p.inflight.erase i,
             resources := i :: p.resources }
  else p

/-- The launch failed: `Reservation::drop` gives the slot back. -/
def fail (p : Pool) (i : SandboxId) : Pool :=
  { p with inflight := p.inflight.erase i }

/-- `reclaim` for every slot that `keep` rejects: each removed slot's
resources are shut down, nothing else is touched. -/
def removeWhere (p : Pool) (keep : Slot → Bool) : Pool :=
  let removed := (p.slots.filter (fun s => !keep s)).map Slot.id
  { p with slots := p.slots.filter keep,
           resources := p.resources.filter (fun r => !removed.contains r) }

/-- `check_owner`: the person may use any sandbox, a session only its own. -/
def authorized (p : Pool) (c : Caller) (i : SandboxId) : Prop :=
  ∃ s ∈ p.slots, s.id = i ∧
    match c with
    | .person => True
    | .session a => s.owner = .session a

def stop (p : Pool) (c : Caller) (i : SandboxId) [Decidable (authorized p c i)] : Pool :=
  if authorized p c i then removeWhere p (fun s => s.id != i) else p

/-- `release_owner`: the session ended. -/
def releaseOwner (p : Pool) (a : Session) : Pool :=
  removeWhere p (fun s => s.owner != .session a)

/-- The reaper: `doomed` covers an ended owner, idleness and a dead backend. -/
def reap (p : Pool) (doomed : Slot → Bool) : Pool :=
  removeWhere p (fun s => !doomed s)

/-- SIGKILLed Engine: every browser exits on pipe EOF; the next pool starts
empty and sweeps the dead Engine's directories. -/
def engineCrash (p : Pool) : Pool := { empty p.capacity with nextId := p.nextId }

/-! ## Lemmas -/

theorem filter_contains_eq {l : List Slot} {keep : Slot → Bool} {R : List SandboxId}
    (hR : ∀ s ∈ l, R.contains s.id = !keep s) :
    (l.map Slot.id).filter (fun r => !R.contains r) = (l.filter keep).map Slot.id := by
  induction l with
  | nil => rfl
  | cons s t ih =>
    have hs := hR s (by simp)
    have ht : ∀ x ∈ t, R.contains x.id = !keep x := fun x hx => hR x (by simp [hx])
    cases hk : keep s <;> simp_all

theorem eq_of_id_eq {l : List Slot} (h : (l.map Slot.id).Nodup) {s t : Slot}
    (hs : s ∈ l) (ht : t ∈ l) (e : s.id = t.id) : s = t := by
  induction l with
  | nil => simp at hs
  | cons x xs ih =>
    rw [List.map_cons, List.nodup_cons] at h
    simp only [List.mem_cons] at hs ht
    rcases hs with rfl | hs <;> rcases ht with rfl | ht
    · rfl
    · exact absurd (e ▸ List.mem_map_of_mem ht) h.1
    · exact absurd (e.symm ▸ List.mem_map_of_mem hs) h.1
    · exact ih h.2 hs ht

theorem removed_iff {l : List Slot} (h : (l.map Slot.id).Nodup) (keep : Slot → Bool) :
    ∀ s ∈ l, ((l.filter (fun s => !keep s)).map Slot.id).contains s.id = !keep s := by
  intro s hs
  cases hk : keep s
  · simp only [Bool.not_false]
    apply List.elem_iff.mpr
    exact List.mem_map_of_mem (List.mem_filter.mpr ⟨hs, by simp [hk]⟩)
  · simp only [Bool.not_true]
    apply Bool.eq_false_iff.mpr
    intro hc
    obtain ⟨t, ht, e⟩ := List.mem_map.mp (List.elem_iff.mp hc)
    obtain ⟨ht, hkt⟩ := List.mem_filter.mp ht
    have := eq_of_id_eq h hs ht e.symm
    subst this
    simp [hk] at hkt

theorem ids_nodup {p : Pool} (h : Inv p) : (ids p).Nodup :=
  (List.nodup_append.mp h.unique).1

/-! ## Invariant preservation -/

theorem removeWhere_inv {p : Pool} (h : Inv p) (keep : Slot → Bool) :
    Inv (removeWhere p keep) := by
  have sub : (p.slots.filter keep).Sublist p.slots := List.filter_sublist
  have subIds : ((p.slots.filter keep).map Slot.id).Sublist (ids p) := sub.map _
  constructor
  · have := sub.length_le
    have := h.bounded
    simp only [removeWhere]
    omega
  · exact (subIds.append (List.Sublist.refl _)).nodup h.unique
  · intro i hi
    exact h.fresh i ((subIds.append (List.Sublist.refl _)).subset hi)
  · simp only [removeWhere, h.backed, ids]
    exact filter_contains_eq (removed_iff (ids_nodup h) keep)

theorem reserve_inv {p q : Pool} {i : SandboxId} (h : Inv p) (r : reserve p = some (q, i)) :
    Inv q ∧ i ∉ ids p := by
  unfold reserve at r
  split at r
  · rename_i room
    simp only [Option.some.injEq, Prod.mk.injEq] at r
    obtain ⟨rfl, rfl⟩ := r
    have notMem : p.nextId ∉ ids p ++ p.inflight := fun m => Nat.lt_irrefl _ (h.fresh _ m)
    refine ⟨⟨?_, ?_, ?_, ?_⟩, fun m => notMem (List.mem_append_left _ m)⟩
    · simp only [List.length_cons]; omega
    · have u := h.unique
      simp only [ids] at notMem u ⊢
      rw [List.nodup_append] at u ⊢
      refine ⟨u.1, List.nodup_cons.mpr ⟨fun m => notMem (List.mem_append_right _ m), u.2.1⟩, ?_⟩
      intro a ha b hb
      simp only [List.mem_cons] at hb
      rcases hb with rfl | hb
      · exact fun e => notMem (List.mem_append_left _ (e ▸ ha))
      · exact u.2.2 a ha b hb
    · intro j hj
      simp only [ids, List.mem_append, List.mem_cons] at hj
      rcases hj with hj | rfl | hj
      · exact Nat.lt_succ_of_lt (h.fresh j (List.mem_append_left _ hj))
      · exact Nat.lt_succ_self _
      · exact Nat.lt_succ_of_lt (h.fresh j (List.mem_append_right _ hj))
    · exact h.backed
  · simp at r

theorem fail_inv {p : Pool} (h : Inv p) (i : SandboxId) : Inv (fail p i) := by
  have sub : (p.inflight.erase i).Sublist p.inflight := List.erase_sublist
  constructor
  · have := sub.length_le; have := h.bounded; simp only [fail]; omega
  · exact ((List.Sublist.refl _).append sub).nodup h.unique
  · intro j hj; exact h.fresh j (((List.Sublist.refl _).append sub).subset hj)
  · exact h.backed

theorem fill_inv {p : Pool} (h : Inv p) (i : SandboxId) (o : Owner) : Inv (fill p i o) := by
  unfold fill
  split
  · rename_i hi
    have lenErase := List.length_erase_of_mem hi
    have perm : (ids p ++ p.inflight).Perm (i :: (ids p ++ p.inflight.erase i)) := by
      have := (List.perm_cons_erase hi)
      exact (List.Perm.append_left _ this).trans List.perm_middle
    constructor
    · have := h.bounded
      have : 0 < p.inflight.length := List.length_pos_of_mem hi
      simp only [List.length_cons]
      omega
    · simpa [ids] using perm.nodup_iff.mp h.unique
    · intro j hj
      apply h.fresh j
      apply perm.symm.subset
      simpa [ids] using hj
    · simp [ids, h.backed]
  · exact h

/-! ## The properties -/

theorem capacity_bound {p : Pool} (h : Inv p) : p.slots.length ≤ p.capacity := by
  have := h.bounded; omega

/-- A full pool refuses a start. -/
theorem full_refuses {p : Pool} (full : p.capacity ≤ p.slots.length + p.inflight.length) :
    reserve p = none := by
  unfold reserve; split <;> first | rfl | omega

theorem lease_exclusive {p : Pool} (h : Inv p) {i : SandboxId} {a b : Session}
    (ha : authorized p (.session a) i) (hb : authorized p (.session b) i) : a = b := by
  obtain ⟨s, hs, si, so⟩ := ha
  obtain ⟨t, ht, ti, to⟩ := hb
  have := eq_of_id_eq (ids_nodup h) hs ht (si.trans ti.symm)
  subst this
  rw [so] at to
  exact Owner.session.inj to

theorem reclaim_leaks_nothing {p : Pool} (h : Inv p) (keep : Slot → Bool) :
    let q := removeWhere p keep
    q.resources = ids q ∧ ∀ s ∈ q.slots, keep s = true := by
  refine ⟨(removeWhere_inv h keep).backed, fun s hs => ?_⟩
  exact (List.mem_filter.mp hs).2

theorem release_leaks_nothing {p : Pool} (h : Inv p) (a : Session) :
    let q := releaseOwner p a
    Inv q ∧ q.resources = ids q ∧ ∀ s ∈ q.slots, s.owner ≠ .session a := by
  refine ⟨removeWhere_inv h _, (removeWhere_inv h _).backed, fun s hs => ?_⟩
  have := (List.mem_filter.mp hs).2
  simpa using this

theorem stop_inv {p : Pool} (h : Inv p) (c : Caller) (i : SandboxId)
    [Decidable (authorized p c i)] : Inv (stop p c i) := by
  unfold stop; split
  · exact removeWhere_inv h _
  · exact h

/-- A session that does not own a sandbox cannot stop it. -/
theorem stop_needs_lease {p : Pool} (c : Caller) (i : SandboxId) [Decidable (authorized p c i)]
    (no : ¬ authorized p c i) : stop p c i = p := by
  unfold stop; simp [no]

theorem reap_inv {p : Pool} (h : Inv p) (doomed : Slot → Bool) : Inv (reap p doomed) :=
  removeWhere_inv h _

theorem engine_crash_leaks_nothing (p : Pool) :
    (engineCrash p).resources = [] ∧ Inv (engineCrash p) := by
  refine ⟨rfl, ?_⟩
  constructor <;> simp [engineCrash, empty, ids]

end SandboxPool
