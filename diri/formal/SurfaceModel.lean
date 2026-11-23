import Std

namespace SurfaceModel

/-! Live Surfaces (`diri_proto::surface`): bounded component trees, atomic
patches that keep the person's state, and the one gate every click passes. -/

-- Actions and the gate (`surface/action.rs`).

inductive Url where
  | diriSession (id : Nat)
  | external (scheme : String) (rest : String)

def allowedSchemes : List String :=
  ["https", "http", "diri", "slack", "linear", "notion", "figma", "mailto"]

inductive Action where
  | openUrl (url : Url)
  | sendPrompt (target : Option Nat) (text : String)
  | event (name : String)
  | refresh

inductive Gesture where
  | user
  | programmatic

structure Authority where
  owner : Option Nat
  agents : List Nat

inductive Effect where
  | openExternal (url : Url)
  | openSession (id : Nat)
  | prompt (session : Nat) (text : String)
  | deliver (session : Nat) (name : String)

/-- The session an effect sends something to, if any. -/
def Effect.recipient : Effect → Option Nat
  | .prompt s _ => some s
  | .deliver s _ => some s
  | _ => none

def Authority.mayReach (a : Authority) (s : Nat) : Bool :=
  a.owner == some s || a.agents.contains s

/-- A requested session, defaulting to the owner, if the surface may reach it. -/
def Authority.target (a : Authority) (requested : Option Nat) : Option Nat :=
  match requested <|> a.owner with
  | some s => if a.mayReach s then some s else none
  | none => none

def authorize : Action → Gesture → Authority → Option Effect
  | _, .programmatic, _ => none
  | .openUrl (.diriSession id), .user, _ => some (.openSession id)
  | .openUrl (.external scheme rest), .user, _ =>
      if allowedSchemes.contains scheme then some (.openExternal (.external scheme rest)) else none
  | .sendPrompt t text, .user, a => (a.target t).map fun s => .prompt s text
  | .event n, .user, a => (a.target none).map fun s => .deliver s n
  | .refresh, .user, a => (a.target none).map fun s => .deliver s "refresh"

theorem authorize_requires_gesture (act : Action) (a : Authority) :
    authorize act .programmatic a = none := by
  cases act with
  | openUrl u => cases u <;> rfl
  | _ => rfl

theorem target_reach {a : Authority} {r : Option Nat} {s : Nat} (h : a.target r = some s) :
    a.owner = some s ∨ s ∈ a.agents := by
  unfold Authority.target at h
  split at h
  · rename_i s' _
    by_cases hr : a.mayReach s' = true
    · simp [hr] at h
      subst h
      simpa [Authority.mayReach] using hr
    · simp [hr] at h
  · simp at h

/-- Prompts, app skills and events reach only the owner or a listed agent. -/
theorem prompt_reaches_only_allowed {act : Action} {g : Gesture} {a : Authority} {e : Effect}
    {s : Nat} (h : authorize act g a = some e) (hr : e.recipient = some s) :
    a.owner = some s ∨ s ∈ a.agents := by
  cases g with
  | programmatic => simp [authorize_requires_gesture] at h
  | user =>
    cases act with
    | openUrl u =>
      cases u with
      | diriSession id => simp [authorize] at h; subst h; simp [Effect.recipient] at hr
      | external sc r =>
        simp only [authorize] at h
        split at h
        · simp at h; subst h; simp [Effect.recipient] at hr
        · simp at h
    | sendPrompt t text =>
      simp only [authorize, Option.map_eq_some_iff] at h
      obtain ⟨s', ht, rfl⟩ := h
      simp [Effect.recipient] at hr; subst hr; exact target_reach ht
    | event n =>
      simp only [authorize, Option.map_eq_some_iff] at h
      obtain ⟨s', ht, rfl⟩ := h
      simp [Effect.recipient] at hr; subst hr; exact target_reach ht
    | refresh =>
      simp only [authorize, Option.map_eq_some_iff] at h
      obtain ⟨s', ht, rfl⟩ := h
      simp [Effect.recipient] at hr; subst hr; exact target_reach ht

/-- Links leave Diri only with an allowed scheme. -/
theorem url_scheme_allowed {act : Action} {g : Gesture} {a : Authority} {u : Url}
    (h : authorize act g a = some (.openExternal u)) :
    ∃ sc r, u = .external sc r ∧ sc ∈ allowedSchemes := by
  cases g with
  | programmatic => simp [authorize_requires_gesture] at h
  | user =>
    cases act with
    | openUrl v =>
      cases v with
      | diriSession id => simp [authorize] at h
      | external sc r =>
        simp only [authorize] at h
        split at h
        · rename_i hs
          simp at h
          exact ⟨sc, r, h.symm, by simpa using hs⟩
        · simp at h
    | sendPrompt t text => simp [authorize, Option.map_eq_some_iff] at h
    | event n => simp [authorize, Option.map_eq_some_iff] at h
    | refresh => simp [authorize, Option.map_eq_some_iff] at h

-- Component trees (`surface/node.rs`, `surface/limits.rs`).

/-- A component: optional id, whether it keeps state (checklist, toggle,
select), whether `insert` may put children in it, an optional button action,
and its children. -/
inductive Node where
  | mk (id : Option Nat) (stateful : Bool) (container : Bool) (action : Option Action)
      (children : List Node)

mutual
def sizeList : List Node → Nat
  | [] => 0
  | .mk _ _ _ _ cs :: ns => 1 + sizeList cs + sizeList ns

def depthList : List Node → Nat
  | [] => 0
  | .mk _ _ _ _ cs :: ns => max (1 + depthList cs) (depthList ns)
end

def idsList : List Node → List Nat
  | [] => []
  | .mk id _ _ _ cs :: ns => id.toList ++ idsList cs ++ idsList ns

/-- Ids whose component keeps state: the keys state may hold. -/
def statefulList : List Node → List Nat
  | [] => []
  | .mk id st _ _ cs :: ns =>
      (if st then id.toList else []) ++ statefulList cs ++ statefulList ns

structure Limits where
  maxNodes : Nat
  maxDepth : Nat

/-- `validate_tree`: within the node and depth bounds, ids unique. -/
def bounded (L : Limits) (root : List Node) : Prop :=
  sizeList root ≤ L.maxNodes ∧ depthList root ≤ L.maxDepth ∧ (idsList root).Nodup

instance (L : Limits) (root : List Node) : Decidable (bounded L root) := by
  unfold bounded; infer_instance

-- Patches (`surface/patch.rs`). Each op fails (none) when its target is missing.

def replaceList (i : Nat) (new : Node) : List Node → Option (List Node)
  | [] => none
  | .mk id st c a cs :: ns =>
    if id = some i then some (new :: ns)
    else match replaceList i new cs with
      | some cs' => some (.mk id st c a cs' :: ns)
      | none => (replaceList i new ns).map (.mk id st c a cs :: ·)

def removeList (i : Nat) : List Node → Option (List Node)
  | [] => none
  | .mk id st c a cs :: ns =>
    if id = some i then some ns
    else match removeList i cs with
      | some cs' => some (.mk id st c a cs' :: ns)
      | none => (removeList i ns).map (.mk id st c a cs :: ·)

def insertList (p : Nat) (new : Node) : List Node → Option (List Node)
  | [] => none
  | .mk id st c a cs :: ns =>
    if id = some p then (if c then some (.mk id st c a (cs ++ [new]) :: ns) else none)
    else match insertList p new cs with
      | some cs' => some (.mk id st c a cs' :: ns)
      | none => (insertList p new ns).map (.mk id st c a cs :: ·)

def Node.withId (i : Nat) : Node → Node
  | .mk _ st c a cs => .mk (some i) st c a cs

inductive Op where
  | replace (i : Nat) (new : Node)
  | remove (i : Nat)
  | insert (parent : Option Nat) (new : Node)
  /-- Field changes: the component keeps its id. -/
  | set (i : Nat) (new : Node)

def applyOp (root : List Node) : Op → Option (List Node)
  | .replace i n => replaceList i n root
  | .remove i => removeList i root
  | .insert none n => some (root ++ [n])
  | .insert (some p) n => insertList p n root
  | .set i n => replaceList i (n.withId i) root

/-- Ops in order; the first failure fails the whole patch. -/
def applyOps (root : List Node) : List Op → Option (List Node)
  | [] => some root
  | op :: ops => (applyOp root op).bind (applyOps · ops)

abbrev State (V : Type) := Nat → Option V

/-- `SurfaceState::pruned_to`: keep only entries for live stateful ids. -/
def prune {V : Type} (live : List Nat) (s : State V) : State V :=
  fun i => if i ∈ live then s i else none

structure Doc (V : Type) where
  root : List Node
  state : State V
  revision : Nat
  auth : Authority

variable {V : Type}

/-- `patch::apply`: run the ops on a copy, gate the result on the bounds,
prune state, bump the revision. -/
def apply (L : Limits) (d : Doc V) (ops : List Op) : Option (Doc V) :=
  match applyOps d.root ops with
  | none => none
  | some root' =>
    if bounded L root' then
      some { root := root', state := prune (statefulList root') d.state,
             revision := d.revision + 1, auth := d.auth }
    else none

/-- What the store keeps: the new surface, or the old one untouched. -/
def commit (d : Doc V) (r : Option (Doc V)) : Doc V := r.getD d

theorem apply_spec {L : Limits} {d d' : Doc V} {ops : List Op} (h : apply L d ops = some d') :
    ∃ root', applyOps d.root ops = some root' ∧ bounded L root' ∧
      d' = { root := root', state := prune (statefulList root') d.state,
             revision := d.revision + 1, auth := d.auth } := by
  unfold apply at h
  split at h
  · simp at h
  · rename_i root' hops
    split at h
    · rename_i hb
      simp at h
      exact ⟨root', hops, hb, h.symm⟩
    · simp at h

/-- Every accepted patch, from any surface, yields a bounded tree. -/
theorem apply_bounded {L : Limits} {d d' : Doc V} {ops : List Op}
    (h : apply L d ops = some d') : bounded L d'.root := by
  obtain ⟨root', _, hb, rfl⟩ := apply_spec h
  exact hb

/-- A refused patch leaves the surface exactly as it was. -/
theorem apply_atomic (L : Limits) (d : Doc V) (ops : List Op) :
    apply L d ops = none ∧ commit d (apply L d ops) = d ∨
    ∃ d', apply L d ops = some d' ∧ bounded L d'.root ∧ commit d (apply L d ops) = d' := by
  cases h : apply L d ops with
  | none => exact Or.inl ⟨rfl, rfl⟩
  | some d' => exact Or.inr ⟨d', rfl, apply_bounded h, rfl⟩

theorem apply_deterministic {L : Limits} {d a b : Doc V} {ops : List Op}
    (ha : apply L d ops = some a) (hb : apply L d ops = some b) : a = b := by
  rw [ha] at hb
  exact Option.some.inj hb

/-- A component the patch keeps keeps the person's state. -/
theorem apply_preserves_state {L : Limits} {d d' : Doc V} {ops : List Op} {i : Nat}
    (h : apply L d ops = some d') (_old : i ∈ statefulList d.root)
    (new : i ∈ statefulList d'.root) : d'.state i = d.state i := by
  obtain ⟨root', _, _, rfl⟩ := apply_spec h
  simp only [prune] at new ⊢
  simp [new]

/-- State only names stateful components of the new tree. -/
theorem apply_state_live {L : Limits} {d d' : Doc V} {ops : List Op} {i : Nat}
    (h : apply L d ops = some d') (hs : d'.state i ≠ none) : i ∈ statefulList d'.root := by
  obtain ⟨root', _, _, rfl⟩ := apply_spec h
  simp only [prune] at hs ⊢
  by_cases hi : i ∈ statefulList root'
  · exact hi
  · simp [hi] at hs

theorem apply_revision {L : Limits} {d d' : Doc V} {ops : List Op}
    (h : apply L d ops = some d') : d'.revision = d.revision + 1 := by
  obtain ⟨root', _, _, rfl⟩ := apply_spec h
  rfl

/-- Patches never change who a surface may reach. -/
theorem apply_keeps_authority {L : Limits} {d d' : Doc V} {ops : List Op}
    (h : apply L d ops = some d') : d'.auth = d.auth := by
  obtain ⟨root', _, _, rfl⟩ := apply_spec h
  rfl

/-- Patches cannot mint effects: on a patched surface every button passes the
same gate as before, so any effect needs the person's gesture and reaches only
the original owner or agents. -/
theorem patched_effects_gated {L : Limits} {d d' : Doc V} {ops : List Op} {act : Action}
    {g : Gesture} {e : Effect} {s : Nat} (h : apply L d ops = some d')
    (he : authorize act g d'.auth = some e) (hr : e.recipient = some s) :
    g = .user ∧ (d.auth.owner = some s ∨ s ∈ d.auth.agents) := by
  rw [apply_keeps_authority h] at he
  refine ⟨?_, prompt_reaches_only_allowed he hr⟩
  cases g with
  | user => rfl
  | programmatic => simp [authorize_requires_gesture] at he

/-- Any sequence of patches, each accepted or refused, from a bounded surface
stays bounded. -/
def applyAll (L : Limits) (d : Doc V) : List (List Op) → Doc V
  | [] => d
  | p :: ps => applyAll L (commit d (apply L d p)) ps

theorem applyAll_bounded {L : Limits} (ps : List (List Op)) :
    ∀ {d : Doc V}, bounded L d.root → bounded L (applyAll L d ps).root := by
  induction ps with
  | nil => intro d hd; exact hd
  | cons p ps ih =>
    intro d hd
    apply ih
    cases h : apply L d p with
    | none => simpa [commit, h] using hd
    | some d' => simpa [commit, h] using apply_bounded h

end SurfaceModel
