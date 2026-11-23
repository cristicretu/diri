import Std
import PromptCache

/-!
# Context Ring

Models of `diri-engine/src/clip/{ring,tab_key,affinity}.rs` and of how the
clip actions reach Agents without growing the cached prompt.
-/

namespace Diri.ContextRing

/-! ## Bounded eviction (`clip/ring.rs`) -/

/-- The fields eviction reads. `bytes` is `ClipRecord::charged_bytes`
    (screenshot plus text). -/
structure Clip where
  id : String
  createdAt : Nat
  bytes : Nat

structure Limits where
  maxClips : Nat
  maxBytes : Nat
  ttl : Nat

def bytesSum (l : List Clip) : Nat := (l.map Clip.bytes).sum

/-- `ring::keep_count` as the kept list. `n` is the number of slots left
    (`index >= max_clips`), `used` the bytes already kept. Nat subtraction
    saturates exactly like `now_ms.saturating_sub(created_at_ms)`. The Rust
    sorts newest first before calling it, so `l` is that sorted ring. -/
def keep (now ttl maxBytes : Nat) : Nat → Nat → List Clip → List Clip
  | 0, _, _ => []
  | _, _, [] => []
  | n + 1, used, c :: cs =>
    if ttl ≤ now - c.createdAt ∨ maxBytes < used + c.bytes then []
    else c :: keep now ttl maxBytes n (used + c.bytes) cs

/-- `ring::evict` on an already sorted ring: (kept, evicted). -/
def evict (lim : Limits) (now : Nat) (l : List Clip) : List Clip × List Clip :=
  let kept := keep now lim.ttl lim.maxBytes lim.maxClips 0 l
  (kept, l.drop kept.length)

section Keep
variable (now ttl maxBytes : Nat)

theorem keep_length_le (l : List Clip) :
    ∀ n used, (keep now ttl maxBytes n used l).length ≤ n := by
  induction l with
  | nil => intro n used; cases n <;> simp [keep]
  | cons c cs ih =>
    intro n used
    cases n with
    | zero => simp [keep]
    | succ n =>
      simp only [keep]
      split
      · simp
      · simpa using ih n (used + c.bytes)

theorem keep_bytes_le (l : List Clip) :
    ∀ n used, used ≤ maxBytes →
      used + bytesSum (keep now ttl maxBytes n used l) ≤ maxBytes := by
  induction l with
  | nil => intro n used h; cases n <;> simpa [keep, bytesSum]
  | cons c cs ih =>
    intro n used h
    cases n with
    | zero => simpa [keep, bytesSum]
    | succ n =>
      simp only [keep]
      split
      · simpa [bytesSum]
      · rename_i hc
        have hu : used + c.bytes ≤ maxBytes := by omega
        have := ih n (used + c.bytes) hu
        simp [bytesSum] at this ⊢
        omega

theorem keep_unexpired (l : List Clip) :
    ∀ n used, ∀ c ∈ keep now ttl maxBytes n used l, now - c.createdAt < ttl := by
  induction l with
  | nil => intro n used c hc; cases n <;> simp [keep] at hc
  | cons d ds ih =>
    intro n used c hc
    cases n with
    | zero => simp [keep] at hc
    | succ n =>
      simp only [keep] at hc
      split at hc
      · simp at hc
      · rename_i hd
        rcases List.mem_cons.mp hc with rfl | hc
        · omega
        · exact ih n _ c hc

theorem keep_prefix (l : List Clip) :
    ∀ n used, keep now ttl maxBytes n used l <+: l := by
  induction l with
  | nil => intro n used; cases n <;> simp [keep]
  | cons c cs ih =>
    intro n used
    cases n with
    | zero => simp [keep]
    | succ n =>
      simp only [keep]
      split
      · exact List.nil_prefix
      · exact List.cons_prefix_cons.mpr ⟨rfl, ih n _⟩

theorem keep_idem (l : List Clip) :
    ∀ n used, keep now ttl maxBytes n used (keep now ttl maxBytes n used l) =
      keep now ttl maxBytes n used l := by
  induction l with
  | nil => intro n used; cases n <;> simp [keep]
  | cons c cs ih =>
    intro n used
    cases n with
    | zero => simp [keep]
    | succ n =>
      simp only [keep]
      split
      · cases n <;> simp [keep]
      · rename_i hc
        simp only [keep, ite_eq_right hc]
        rw [ih n]

end Keep

theorem evict_count (lim : Limits) (now : Nat) (l : List Clip) :
    (evict lim now l).1.length ≤ lim.maxClips :=
  keep_length_le _ _ _ l _ _

theorem evict_bytes (lim : Limits) (now : Nat) (l : List Clip) :
    bytesSum (evict lim now l).1 ≤ lim.maxBytes := by
  have := keep_bytes_le now lim.ttl lim.maxBytes l lim.maxClips 0 (Nat.zero_le _)
  simpa [evict] using this

theorem evict_unexpired (lim : Limits) (now : Nat) (l : List Clip) :
    ∀ c ∈ (evict lim now l).1, now - c.createdAt < lim.ttl :=
  keep_unexpired _ _ _ l _ _

/-- The newest clips survive: kept is a prefix and kept ++ evicted is the ring. -/
theorem evict_prefix (lim : Limits) (now : Nat) (l : List Clip) :
    (evict lim now l).1 <+: l ∧ (evict lim now l).1 ++ (evict lim now l).2 = l := by
  have h := keep_prefix now lim.ttl lim.maxBytes l lim.maxClips 0
  refine ⟨h, ?_⟩
  obtain ⟨t, ht⟩ := h
  simp only [evict]
  have hd : l.drop (keep now lim.ttl lim.maxBytes lim.maxClips 0 l).length = t := by
    calc l.drop (keep now lim.ttl lim.maxBytes lim.maxClips 0 l).length
        = ((keep now lim.ttl lim.maxBytes lim.maxClips 0 l) ++ t).drop
            (keep now lim.ttl lim.maxBytes lim.maxClips 0 l).length := by rw [ht]
      _ = t := List.drop_left
  rw [hd, ht]

/-- Evicting what was kept keeps all of it and evicts nothing. -/
theorem evict_idem (lim : Limits) (now : Nat) (l : List Clip) :
    evict lim now (evict lim now l).1 = ((evict lim now l).1, []) := by
  simp [evict, keep_idem]

/-! ## Canonical tab keys (`clip/tab_key.rs`)

Modeled at the segment and pair level: a path is its `/`-separated segments,
a query its `(name, value)` pairs. Rendering back to text and re-splitting
is not modeled. -/

/-- `normalize_path`: drop empty segments (`//`, leading and trailing `/`). -/
def normalizePath (segments : List String) : List String :=
  segments.filter (· != "")

theorem normalizePath_idem (s : List String) :
    normalizePath (normalizePath s) = normalizePath s := by
  simp [normalizePath, List.filter_filter]

/-- Adjacent dedup, as Rust's `Vec::dedup`. -/
def dedupAdj {α} [DecidableEq α] : List α → List α
  | a :: b :: t => if a = b then dedupAdj (b :: t) else a :: dedupAdj (b :: t)
  | l => l

def NoAdj {α} : List α → Prop
  | a :: b :: t => a ≠ b ∧ NoAdj (b :: t)
  | _ => True

section Dedup
variable {α : Type} [DecidableEq α]

theorem dedupAdj_head (b : α) : ∀ t, ∃ r, dedupAdj (b :: t) = b :: r
  | [] => ⟨[], rfl⟩
  | c :: t => by
    simp only [dedupAdj]
    split
    · rename_i h; subst h; exact dedupAdj_head b t
    · exact ⟨_, rfl⟩

theorem dedupAdj_noAdj : ∀ l : List α, NoAdj (dedupAdj l)
  | [] => trivial
  | [_] => trivial
  | a :: b :: t => by
    simp only [dedupAdj]
    split
    · exact dedupAdj_noAdj (b :: t)
    · rename_i h
      obtain ⟨r, hr⟩ := dedupAdj_head b t
      have ih := dedupAdj_noAdj (b :: t)
      rw [hr] at ih ⊢
      exact ⟨h, ih⟩

theorem dedupAdj_of_noAdj : ∀ l : List α, NoAdj l → dedupAdj l = l
  | [], _ => rfl
  | [_], _ => rfl
  | a :: b :: t, ⟨h, hn⟩ => by
    simp only [dedupAdj, ite_eq_right h]
    rw [dedupAdj_of_noAdj (b :: t) hn]

theorem dedupAdj_sublist : ∀ l : List α, List.Sublist (dedupAdj l) l
  | [] => List.Sublist.slnil
  | [_] => List.Sublist.refl _
  | a :: b :: t => by
    simp only [dedupAdj]
    split
    · exact (dedupAdj_sublist (b :: t)).cons a
    · exact (dedupAdj_sublist (b :: t)).cons_cons a

end Dedup

/-- `(name, value)` lexicographic order, as Rust sorts `(&str, &str)`. -/
def pairLe (a b : String × String) : Bool :=
  decide (a.1 < b.1) || (decide (a.1 = b.1) && decide (a.2 ≤ b.2))

theorem pairLe_trans (a b c : String × String) :
    pairLe a b → pairLe b c → pairLe a c := by
  simp only [pairLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq]
  rintro (h1 | ⟨h1, h1'⟩) (h2 | ⟨h2, h2'⟩)
  · exact Or.inl (Std.lt_trans h1 h2)
  · exact Or.inl (h2 ▸ h1)
  · exact Or.inl (h1 ▸ h2)
  · exact Or.inr ⟨h1.trans h2, Std.le_trans h1' h2'⟩

theorem pairLe_total (a b : String × String) : pairLe a b || pairLe b a := by
  simp only [pairLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq]
  by_cases h1 : a.1 < b.1
  · exact Or.inl (Or.inl h1)
  by_cases h2 : b.1 < a.1
  · exact Or.inr (Or.inl h2)
  have h : a.1 = b.1 := Std.le_antisymm (Std.not_lt.mp h2) (Std.not_lt.mp h1)
  rcases (Std.le_total : a.2 ≤ b.2 ∨ b.2 ≤ a.2) with h' | h'
  · exact Or.inl (Or.inr ⟨h, h'⟩)
  · exact Or.inr (Or.inr ⟨h.symm, h'⟩)

/-- A parameter to keep: a non-empty name that is not tracking
    (`utm_*`, `fbclid`, …; `isTracking` abstracts the list). -/
def keepParam (isTracking : String → Bool) (p : String × String) : Bool :=
  p.1 != "" && !isTracking p.1

/-- `normalize_query`: filter, sort by (name, value), dedup. -/
def normalizeQuery (isTracking : String → Bool) (q : List (String × String)) :
    List (String × String) :=
  dedupAdj ((q.filter (keepParam isTracking)).mergeSort pairLe)

theorem normalizeQuery_sorted (isTracking : String → Bool) (q : List (String × String)) :
    (normalizeQuery isTracking q).Pairwise (fun a b => pairLe a b) :=
  (List.pairwise_mergeSort pairLe_trans pairLe_total
      (q.filter (keepParam isTracking))).sublist
    (dedupAdj_sublist ((q.filter (keepParam isTracking)).mergeSort pairLe))

theorem normalizeQuery_kept (isTracking : String → Bool) (q : List (String × String)) :
    ∀ p ∈ normalizeQuery isTracking q, keepParam isTracking p := by
  intro p hp
  have := (dedupAdj_sublist ((q.filter (keepParam isTracking)).mergeSort pairLe)).subset hp
  rw [List.mem_mergeSort] at this
  exact (List.mem_filter.mp this).2

theorem normalizeQuery_idem (isTracking : String → Bool) (q : List (String × String)) :
    normalizeQuery isTracking (normalizeQuery isTracking q) = normalizeQuery isTracking q := by
  rw [show normalizeQuery isTracking (normalizeQuery isTracking q) =
      dedupAdj (((normalizeQuery isTracking q).filter (keepParam isTracking)).mergeSort pairLe)
      from rfl,
    List.filter_eq_self.mpr (normalizeQuery_kept isTracking q),
    List.mergeSort_of_pairwise (normalizeQuery_sorted isTracking q)]
  exact dedupAdj_of_noAdj _ (dedupAdj_noAdj _)

/-! ## Per-tab Session memory (`clip/affinity.rs`) -/

structure Entry where
  key : String
  session : String
  time : Nat

/-- `tab_key::lookup_chain`: the key itself, then its parent pages. -/
def chainOf (parents : String → List String) (k : String) : List String :=
  k :: parents k

def recordOne (s : String) (t : Nat) (es : List Entry) (k : String) : List Entry :=
  ⟨k, s, t⟩ :: es.filter (·.key != k)

/-- `Affinity::record`: parents first so the exact key ends most recent,
    each write replacing that key's old entry, then truncate to `cap` (`MAX_ENTRIES`). -/
def record (parents : String → List String) (cap : Nat)
    (es : List Entry) (k s : String) (t : Nat) : List Entry :=
  ((chainOf parents k).reverse.foldl (recordOne s t) es).take cap

/-- `Affinity::lookup`: the first chain key whose entry names a live Session. -/
def lookup (parents : String → List String) (alive : String → Bool)
    (es : List Entry) (k : String) : Option String :=
  (chainOf parents k).findSome? fun k' =>
    ((es.find? (·.key == k')).map (·.session)).filter alive

theorem record_length (parents : String → List String) (cap : Nat) (es : List Entry)
    (k s : String) (t : Nat) :
    (record parents cap es k s t).length ≤ cap :=
  List.length_take_le _ _

theorem record_head (parents : String → List String) (cap : Nat) (es : List Entry)
    (k s : String) (t : Nat) (hmax : 0 < cap) :
    ∃ rest, record parents cap es k s t = ⟨k, s, t⟩ :: rest := by
  obtain ⟨m, rfl⟩ : ∃ m, cap = m + 1 := ⟨cap - 1, by omega⟩
  simp [record, chainOf, List.foldl_append, recordOne]

/-- Lookup after record finds the recorded Session (any `max ≥ 1`). -/
theorem lookup_record (parents : String → List String) (cap : Nat) (es : List Entry)
    (k s : String) (t : Nat) (hmax : 0 < cap) :
    lookup parents (fun _ => true) (record parents cap es k s t) k = some s := by
  obtain ⟨rest, h⟩ := record_head parents cap es k s t hmax
  rw [h]
  simp [lookup, chainOf]

theorem find?_filter_of_imp {α} (p q : α → Bool) (h : ∀ a, p a → q a) :
    ∀ l : List α, (l.filter q).find? p = l.find? p
  | [] => rfl
  | a :: l => by
    by_cases hp : p a
    · simp [List.filter, h a hp, hp]
    · by_cases hq : q a
      · simp [List.filter, hq, hp, find?_filter_of_imp p q h l]
      · simp [List.filter, hq, hp, find?_filter_of_imp p q h l]

theorem findSome?_congr {α β} (f g : α → Option β) :
    ∀ l : List α, (∀ a ∈ l, f a = g a) → l.findSome? f = l.findSome? g
  | [], _ => rfl
  | a :: l, h => by
    simp only [List.findSome?_cons, h a (List.mem_cons_self ..),
      findSome?_congr f g l (fun b hb => h b (List.mem_cons_of_mem _ hb))]

/-- Lookup reads only entries for keys on the page's own chain. -/
theorem lookup_local (parents : String → List String) (alive : String → Bool)
    (es : List Entry) (k : String) :
    lookup parents alive es k =
      lookup parents alive (es.filter (fun e => (chainOf parents k).contains e.key)) k := by
  unfold lookup
  apply findSome?_congr
  intro k' hk'
  rw [find?_filter_of_imp]
  intro e he
  simp only [beq_iff_eq] at he
  simp [he, hk']

/-! ## Zero prompt-cache overhead

Clip actions are deferred: reachable through `get_skill {action}` and `act`
(and `tools/call` by name) but absent from `tools/list`, the instructions and
the skills, which is all the cached prompt sees. -/

structure Catalog where
  visible : List String
  deferred : List String
  instructions : String
  skills : List String

def clipActions : List String := ["list_clips", "get_clip", "capture_clip", "send_clip"]

/-- What `tools/list`, `initialize` and the skills plugin put in the prompt. -/
def visiblePrompt (c : Catalog) : List String × String × List String :=
  (c.visible, c.instructions, c.skills)

/-- What `act` / `tools/call` accept. -/
def callable (c : Catalog) : List String := c.visible ++ c.deferred

theorem deferred_invisible (c : Catalog) (d : List String) :
    visiblePrompt { c with deferred := d } = visiblePrompt c := rfl

theorem deferred_callable (c : Catalog) (a : String) (h : a ∈ c.deferred) :
    a ∈ callable c := List.mem_append_right _ h

theorem clip_actions_callable (c : Catalog) (h : c.deferred = clipActions) :
    ∀ a ∈ clipActions, a ∈ callable c := fun a ha => deferred_callable c a (h ▸ ha)

/-- Tie-in with `PromptCache.lean`: the Claude launch is built from the
    visible schemas only, so adding deferred actions changes nothing an
    Agent's cached prompt contains. -/
def project (c : Catalog) (base : Diri.Claude.Project) : Diri.Claude.Project :=
  { base with toolSchemas := c.visible }

theorem inject_ignores_deferred (m p base s) (c : Catalog) (d : List String) :
    (Diri.Claude.inject m p (project { c with deferred := d } base) s).visible =
      (Diri.Claude.inject m p (project c base) s).visible := rfl

end Diri.ContextRing
