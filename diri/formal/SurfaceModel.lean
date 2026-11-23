import Std

namespace SurfaceModel

/-! Live Surfaces (`diri_proto::surface`): bounded component trees, atomic
patches that keep the person's state, and the one gate every click passes. -/

-- Actions and the gate (`surface/action.rs`).

inductive Url where
  | diriSession (id : Nat)
  | external (scheme : String) (rest : String)

def allowedSchemes : List String :=
  ["https", "http", "mailto", "diri"]

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

/-! ## Lua apps (`diri_script::app`)

An app is `app.lua`: `render` returns a tree, `on_event` returns new state and
actions, `sync` returns new data. Each call runs in a fresh sandbox with an
instruction budget, off the UI thread. -/

section LuaApps

/-- A program as a step function: each step either continues or finishes. -/
structure Prog (S O : Type) where
  start : S
  step : S → S ⊕ O

variable {S O : Type}

/-- At most `fuel` steps; running out is a refusal, never a hang. -/
def runFrom (step : S → S ⊕ O) : Nat → S → Option O
  | 0, _ => none
  | n + 1, s =>
    match step s with
    | .inr o => some o
    | .inl s' => runFrom step n s'

def run (fuel : Nat) (p : Prog S O) : Option O := runFrom p.step fuel p.start

/-- How many steps a run takes before it finishes or runs out. -/
def stepsUsed (step : S → S ⊕ O) : Nat → S → Nat
  | 0, _ => 0
  | n + 1, s =>
    match step s with
    | .inr _ => 1
    | .inl s' => 1 + stepsUsed step n s'

/-- The budget bounds the work: a run never takes more steps than its fuel. -/
theorem run_bounded_steps (step : S → S ⊕ O) :
    ∀ (fuel : Nat) (s : S), stepsUsed step fuel s ≤ fuel := by
  intro fuel
  induction fuel with
  | zero => intro s; simp [stepsUsed]
  | succ n ih =>
    intro s
    simp only [stepsUsed]
    split
    · omega
    · have := ih ‹S›; omega

theorem run_no_fuel (p : Prog S O) : run 0 p = none := rfl

/-- `render`: whatever Lua returns is drawn only if it passes the same bounds
as every other surface. -/
def renderApp (fuel : Nat) (L : Limits) (p : Prog S (List Node)) : Option (List Node) :=
  (run fuel p).bind fun t => if bounded L t then some t else none

theorem render_result_bounded {fuel : Nat} {L : Limits} {p : Prog S (List Node)}
    {t : List Node} (h : renderApp fuel L p = some t) : bounded L t := by
  unfold renderApp at h
  cases hr : run fuel p with
  | none => simp [hr] at h
  | some t' =>
    simp only [hr, Option.bind_some] at h
    split at h
    · rename_i hb; simp at h; subst h; exact hb
    · simp at h

theorem render_no_fuel (L : Limits) (p : Prog S (List Node)) : renderApp 0 L p = none := rfl

/-- `ctx.exec` and `ctx.fetch` run only for the approved digest of the
current `app.lua`. -/
def mayExec {Code D : Type} (approvals : Nat → Option D) (digest : Code → D) (id : Nat)
    (code : Code) : Prop :=
  approvals id = some (digest code)

theorem no_approval_no_exec {Code D : Type} {approvals : Nat → Option D} {digest : Code → D}
    {id : Nat} {code : Code} (h : approvals id = none) : ¬ mayExec approvals digest id code := by
  intro hm; unfold mayExec at hm; rw [h] at hm; cases hm

/-- Editing `app.lua` invalidates its approval, for a collision-free digest
(stated as a premise, not assumed). -/
theorem edit_invalidates_approval {Code D : Type} {approvals : Nat → Option D}
    {digest : Code → D} {id : Nat} {code code' : Code} (hinj : Function.Injective digest)
    (h : mayExec approvals digest id code) (hne : code' ≠ code) :
    ¬ mayExec approvals digest id code' := by
  intro h'
  unfold mayExec at h h'
  rw [h] at h'
  exact hne (hinj (Option.some.inj h')).symm

/-- What the view sees: shown, hidden, time passing, and its own draws. -/
inductive Ev where
  | shown
  | hidden
  | tick (t : Nat)
  | render (t : Nat)

structure Sched where
  visible : Bool
  nextDue : Nat

/-- A sync run starts only from a draw of a visible view that is due; the
draw schedules the next one. A hidden view is never drawn. -/
def stepSched (every : Nat) (s : Sched) : Ev → Sched × Bool
  | .shown => ({ s with visible := true }, false)
  | .hidden => ({ s with visible := false }, false)
  | .tick _ => (s, false)
  | .render t =>
    if s.visible && decide (s.nextDue ≤ t) then ({ s with nextDue := t + every }, true)
    else (s, false)

/-- The scheduler state at each sync run in a trace. -/
def runs (every : Nat) : Sched → List Ev → List Sched
  | _, [] => []
  | s, e :: es =>
    let r := stepSched every s e
    (if r.2 then [s] else []) ++ runs every r.1 es

theorem runs_only_while_visible (every : Nat) :
    ∀ (evs : List Ev) (s : Sched), ∀ r ∈ runs every s evs, r.visible = true := by
  intro evs
  induction evs with
  | nil => intro s r hr; simp [runs] at hr
  | cons e es ih =>
    intro s r hr
    simp only [runs, List.mem_append] at hr
    rcases hr with hr | hr
    · cases e with
      | shown => simp [stepSched] at hr
      | hidden => simp [stepSched] at hr
      | tick t => simp [stepSched] at hr
      | render t =>
        simp only [stepSched] at hr
        split at hr
        · rename_i hv
          simp at hr; subst hr
          simp at hv; exact hv.1
        · simp at hr
    · exact ih _ r hr

/-- Hidden with no later show: nothing runs, whatever else happens. -/
theorem hidden_schedules_nothing (every : Nat) :
    ∀ (evs : List Ev) (s : Sched), s.visible = false → (∀ e ∈ evs, e ≠ .shown) →
      runs every s evs = [] := by
  intro evs
  induction evs with
  | nil => intro s _ _; rfl
  | cons e es ih =>
    intro s hs hno
    have hrest : ∀ e' ∈ es, e' ≠ .shown := fun e' he' => hno e' (List.mem_cons_of_mem e he')
    cases e with
    | shown => exact absurd rfl (hno .shown (List.mem_cons_self))
    | hidden => simp only [runs, stepSched]; simpa using ih _ rfl hrest
    | tick t => simp only [runs, stepSched]; simpa using ih s hs hrest
    | render t =>
      simp only [runs, stepSched, hs, Bool.false_and]
      simpa using ih s hs hrest

theorem hide_then_nothing (every : Nat) (s : Sched) (evs : List Ev)
    (hno : ∀ e ∈ evs, e ≠ .shown) : runs every s (.hidden :: evs) = [] := by
  simp only [runs, stepSched]
  simpa using hidden_schedules_nothing every evs _ rfl hno

/-- Effects of one app: actions `on_event` returned for a click pass the gate
as the person's; anything `render` or `sync` returns passes it as nobody's. -/
def appEffects (a : Authority) (clicked rendered synced : List Action) : List Effect :=
  clicked.filterMap (fun act => authorize act .user a) ++
    (rendered ++ synced).filterMap (fun act => authorize act .programmatic a)

theorem render_sync_mint_no_effects (a : Authority) (rendered synced : List Action) :
    (rendered ++ synced).filterMap (fun act => authorize act .programmatic a) = [] := by
  simp [authorize_requires_gesture]

theorem event_effects_gated {a : Authority} {clicked rendered synced : List Action} {e : Effect}
    (h : e ∈ appEffects a clicked rendered synced) :
    (∀ s, e.recipient = some s → a.owner = some s ∨ s ∈ a.agents) ∧
      (∀ u, e = .openExternal u → ∃ sc r, u = .external sc r ∧ sc ∈ allowedSchemes) := by
  unfold appEffects at h
  rw [render_sync_mint_no_effects, List.append_nil, List.mem_filterMap] at h
  obtain ⟨act, _, he⟩ := h
  exact ⟨fun s hr => prompt_reaches_only_allowed he hr,
    fun u hu => by subst hu; exact url_scheme_allowed he⟩

end LuaApps

end SurfaceModel
