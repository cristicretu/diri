import Std

namespace DualLens

/-! # Dual-Lens thread observation

The Engine tails an Agent's append-only JSONL transcript by byte offset.
A file is a list of bytes; `newline` (byte 10) ends a record. -/

def newline : Nat := 10

/-- One emitted transcript line: its starting byte offset (the key) and its bytes
    without the terminating newline. -/
abbrev Item := Nat × List Nat

/-- Scanner state of `thread::tail::read_window`: emitted items, the start of the
    current (partial) line, that line's bytes reversed, and the byte position. -/
structure St where
  items : List Item
  start : Nat
  cur : List Nat
  pos : Nat

def step (s : St) (b : Nat) : St :=
  if b = newline then ⟨s.items ++ [(s.start, s.cur.reverse)], s.pos + 1, [], s.pos + 1⟩
  else ⟨s.items, s.start, b :: s.cur, s.pos + 1⟩

def init (o : Nat) : St := ⟨[], o, [], o⟩

def scan (f : List Nat) (o : Nat) : St := (f.drop o).foldl step (init o)

/-- `session.thread_read(cursor)`: every complete line from `o` to EOF keyed by its
    start offset, and `next` = the offset just past the last newline consumed. -/
def read (f : List Nat) (o : Nat) : List Item × Nat :=
  ((scan f o).items, (scan f o).start)

/-! ## Scanner lemmas -/

theorem fold_pos (xs : List Nat) (s : St) : (xs.foldl step s).pos = s.pos + xs.length := by
  induction xs generalizing s with
  | nil => simp
  | cons b bs ih =>
    rw [List.foldl_cons, ih]
    by_cases h : b = newline <;> simp [step, h] <;> omega

theorem fold_no_newline (zs : List Nat) (s : St) (h : newline ∉ zs) :
    (zs.foldl step s).items = s.items ∧ (zs.foldl step s).start = s.start := by
  induction zs generalizing s with
  | nil => simp
  | cons b bs ih =>
    have hb : b ≠ newline := fun e => h (e ▸ List.mem_cons_self)
    have ht : newline ∉ bs := fun m => h (List.mem_cons_of_mem _ m)
    rw [List.foldl_cons]
    simpa [step, hb] using ih (step s b) ht

/-- Prefixing already-emitted items commutes with scanning. -/
def pre (I : List Item) (s : St) : St := { s with items := I ++ s.items }

theorem fold_pre (I : List Item) (xs : List Nat) (s : St) :
    xs.foldl step (pre I s) = pre I (xs.foldl step s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons b bs ih =>
    have : step (pre I s) b = pre I (step s b) := by
      by_cases h : b = newline <;> simp [step, pre, h]
    rw [List.foldl_cons, List.foldl_cons, this, ih]

/-- A state at a record boundary: no partial line pending. -/
def Clean (s : St) : Prop := s.cur = [] ∧ s.pos = s.start

theorem clean_eq (s : St) (h : Clean s) : s = pre s.items (init s.start) := by
  obtain ⟨i, st, c, p⟩ := s
  obtain ⟨hc, hp⟩ := h
  simp_all [pre, init]

theorem fold_end_newline (ys : List Nat) (s : St) :
    Clean ((ys ++ [newline]).foldl step s) ∧
    ((ys ++ [newline]).foldl step s).start = s.pos + ys.length + 1 := by
  rw [List.foldl_append]
  simp [step, Clean, fold_pos]

/-- Every byte list splits at its last newline. -/
theorem last_newline_split (xs : List Nat) :
    ∃ ys zs, xs = ys ++ zs ∧ newline ∉ zs ∧ (ys = [] ∨ ∃ ys', ys = ys' ++ [newline]) := by
  induction xs with
  | nil => exact ⟨[], [], rfl, by simp, Or.inl rfl⟩
  | cons b bs ih =>
    obtain ⟨ys, zs, hxs, hz, hy⟩ := ih
    rcases hy with rfl | ⟨ys', rfl⟩
    · by_cases hb : b = newline
      · exact ⟨[newline], zs, by simp [hxs, hb], hz, Or.inr ⟨[], rfl⟩⟩
      · refine ⟨[], b :: zs, by simp [hxs], ?_, Or.inl rfl⟩
        simp only [List.mem_cons, not_or]
        exact ⟨fun e => hb e.symm, hz⟩
    · exact ⟨b :: ys' ++ [newline], zs, by simp [hxs], hz, Or.inr ⟨b :: ys', rfl⟩⟩

/-- `read` consumes exactly the bytes up to the last newline after `o`. -/
theorem read_decomp (f : List Nat) (o : Nat) (ys zs : List Nat)
    (hxs : f.drop o = ys ++ zs) (hz : newline ∉ zs)
    (hy : ys = [] ∨ ∃ ys', ys = ys' ++ [newline]) :
    Clean (ys.foldl step (init o)) ∧ (ys.foldl step (init o)).start = o + ys.length ∧
    read f o = ((ys.foldl step (init o)).items, o + ys.length) := by
  have hc : Clean (ys.foldl step (init o)) ∧ (ys.foldl step (init o)).start = o + ys.length := by
    rcases hy with rfl | ⟨ys', rfl⟩
    · simp [Clean, init]
    · have := fold_end_newline ys' (init o)
      simp only [init] at this
      simpa [init, Nat.add_assoc] using this
  refine ⟨hc.1, hc.2, ?_⟩
  have hs : scan f o = zs.foldl step (ys.foldl step (init o)) := by
    rw [scan, hxs, List.foldl_append]
  have hn := fold_no_newline zs (ys.foldl step (init o)) hz
  simp [read, hs, hn.1, hn.2, hc.2]

/-! ## 1. Monotone and bounded -/

theorem fold_start_bounds (xs : List Nat) (s : St) (h : s.start ≤ s.pos) :
    s.start ≤ (xs.foldl step s).start ∧ (xs.foldl step s).start ≤ (xs.foldl step s).pos := by
  induction xs generalizing s with
  | nil => simpa using h
  | cons b bs ih =>
    rw [List.foldl_cons]
    have hs : s.start ≤ (step s b).start ∧ (step s b).start ≤ (step s b).pos := by
      by_cases hb : b = newline <;> simp [step, hb] <;> omega
    have := ih (step s b) hs.2
    exact ⟨Nat.le_trans hs.1 this.1, this.2⟩

theorem read_monotone (f : List Nat) (o : Nat) :
    o ≤ (read f o).2 ∧ (read f o).2 ≤ max o f.length := by
  have hb := fold_start_bounds (f.drop o) (init o) (Nat.le_refl _)
  have hp := fold_pos (f.drop o) (init o)
  simp only [read, scan, init] at *
  simp only [List.length_drop] at hp
  omega

theorem read_le_length (f : List Nat) (o : Nat) (h : o ≤ f.length) :
    (read f o).2 ≤ f.length := by
  have := (read_monotone f o).2
  omega

/-! ## 2. The returned cursor is a record boundary -/

/-- A valid cursor: 0, or just past a newline. -/
def Boundary (f : List Nat) (o : Nat) : Prop := o = 0 ∨ (0 < o ∧ f[o - 1]? = some newline)

/-- `thread::tail::cursor_is_boundary`. -/
def isBoundary (f : List Nat) (o : Nat) : Bool := o == 0 || f[o - 1]? == some newline

theorem isBoundary_iff (f : List Nat) (o : Nat) : isBoundary f o = true ↔ Boundary f o := by
  unfold isBoundary Boundary
  by_cases h : o = 0
  · simp [h]
  · simp [h]; omega

theorem Boundary.le_length {f : List Nat} {o : Nat} (h : Boundary f o) : o ≤ f.length := by
  rcases h with rfl | ⟨hp, hs⟩
  · exact Nat.zero_le _
  · have : o - 1 < f.length := by
      apply Classical.byContradiction
      intro hn
      rw [List.getElem?_eq_none (by omega)] at hs
      cases hs
    omega

theorem read_boundary (f : List Nat) (o : Nat) :
    (read f o).2 = o ∨ (0 < (read f o).2 ∧ f[(read f o).2 - 1]? = some newline) := by
  obtain ⟨ys, zs, hxs, hz, hy⟩ := last_newline_split (f.drop o)
  have hr := (read_decomp f o ys zs hxs hz hy).2.2
  rw [hr]
  rcases hy with rfl | ⟨ys', rfl⟩
  · simp
  · right
    have h1 : (f.drop o)[ys'.length]? = some newline := by rw [hxs]; simp
    rw [List.getElem?_drop] at h1
    simp only [List.length_append, List.length_singleton]
    refine ⟨by omega, ?_⟩
    have : o + (ys'.length + 1) - 1 = o + ys'.length := by omega
    rw [this, h1]

theorem read_preserves_boundary (f : List Nat) (o : Nat) (h : Boundary f o) :
    Boundary f (read f o).2 := by
  rcases read_boundary f o with e | e
  · rw [e]; exact h
  · exact Or.inr e

/-! ## 3. Chunk independence: zero duplicates, zero drops -/

/-- Reading `f ++ g` from `o` equals reading `f` from `o` and then reading the grown
    file from the returned cursor. -/
theorem read_append (f g : List Nat) (o : Nat) (h : o ≤ f.length) :
    read (f ++ g) o =
      ((read f o).1 ++ (read (f ++ g) (read f o).2).1, (read (f ++ g) (read f o).2).2) := by
  obtain ⟨ys, zs, hxs, hz, hy⟩ := last_newline_split (f.drop o)
  obtain ⟨hclean, hstart, hr⟩ := read_decomp f o ys zs hxs hz hy
  rw [hr]
  generalize hs₁ : ys.foldl step (init o) = s₁ at hclean hstart
  have hlen : (f.drop o).length = ys.length + zs.length := by rw [hxs]; simp
  have hq : o + ys.length ≤ f.length := by simp at hlen; omega
  have hdrop : (f ++ g).drop (o + ys.length) = zs ++ g := by
    rw [List.drop_append_of_le_length hq, ← List.drop_drop, hxs]
    simp
  have hfull : scan (f ++ g) o = pre s₁.items ((zs ++ g).foldl step (init (o + ys.length))) := by
    rw [scan, List.drop_append_of_le_length h, hxs, List.append_assoc, List.foldl_append, hs₁,
      clean_eq s₁ hclean, fold_pre, hstart]
    simp [pre, init]
  have hrest : scan (f ++ g) (o + ys.length) = (zs ++ g).foldl step (init (o + ys.length)) := by
    rw [scan, hdrop]
  simp [read, hfull, hrest, pre]

/-- The brief's form: one read of the old file, then one read of the grown file from
    the returned cursor, yields exactly one read of the grown file. -/
theorem read_append_split (f₁ g : List Nat) :
    let r₁ := read f₁ 0
    let r₂ := read (f₁ ++ g) r₁.2
    r₁.1 ++ r₂.1 = (read (f₁ ++ g) 0).1 ∧ r₂.2 = (read (f₁ ++ g) 0).2 := by
  simp only
  rw [read_append f₁ g 0 (Nat.zero_le _)]
  exact ⟨rfl, rfl⟩

/-- Tail an append-only file: read, let the writer append the next chunk, read again
    from the returned cursor, and so on. -/
def tailLoop : List Nat → Nat → List (List Nat) → List Item × Nat
  | f, o, [] => read f o
  | f, o, g :: gs =>
    let r := read f o
    let r' := tailLoop (f ++ g) r.2 gs
    (r.1 ++ r'.1, r'.2)

theorem tail_chunking (f : List Nat) (o : Nat) (gs : List (List Nat)) (h : o ≤ f.length) :
    tailLoop f o gs = read (f ++ gs.flatten) o := by
  induction gs generalizing f o with
  | nil => simp [tailLoop]
  | cons g gs ih =>
    have hle : (read f o).2 ≤ (f ++ g).length := by
      have := read_le_length f o h
      simp; omega
    simp only [tailLoop]
    rw [ih (f ++ g) (read f o).2 hle, List.flatten_cons,
      read_append f (g ++ gs.flatten) o h, List.append_assoc]

/-- Any chunking of the writer's appends into reads yields one read of the whole file. -/
theorem tail_chunking_matches_one_read (gs : List (List Nat)) :
    tailLoop [] 0 gs = read gs.flatten 0 := by
  simpa using tail_chunking [] 0 gs (Nat.le_refl _)

/-! ## 4. Keys are strictly increasing and at least the cursor -/

def KeyLt {α : Type} (a b : Nat × α) : Prop := a.1 < b.1

def Inv (lo : Nat) (s : St) : Prop :=
  s.items.Pairwise KeyLt ∧ (∀ x ∈ s.items, lo ≤ x.1 ∧ x.1 < s.start) ∧
    lo ≤ s.start ∧ s.start ≤ s.pos

theorem inv_step (lo : Nat) (s : St) (b : Nat) (h : Inv lo s) : Inv lo (step s b) := by
  obtain ⟨hp, hk, hl, hs⟩ := h
  by_cases hb : b = newline
  · simp only [Inv, step, hb, ↓reduceIte]
    refine ⟨?_, ?_, by omega, Nat.le_refl _⟩
    · rw [List.pairwise_append]
      refine ⟨hp, by simp, ?_⟩
      intro a ha c hc
      simp only [List.mem_singleton] at hc
      subst hc
      exact (hk a ha).2
    · intro x hx
      rcases List.mem_append.mp hx with hx | hx
      · have := hk x hx; omega
      · simp only [List.mem_singleton] at hx; subst hx; simp; omega
  · simp only [Inv, step, hb, ↓reduceIte]
    exact ⟨hp, hk, hl, by omega⟩

theorem inv_fold (lo : Nat) (xs : List Nat) (s : St) (h : Inv lo s) :
    Inv lo (xs.foldl step s) := by
  induction xs generalizing s with
  | nil => exact h
  | cons b bs ih => exact ih _ (inv_step lo s b h)

theorem read_keys (f : List Nat) (o : Nat) :
    (read f o).1.Pairwise KeyLt ∧ ∀ x ∈ (read f o).1, o ≤ x.1 ∧ x.1 < (read f o).2 := by
  have := inv_fold o (f.drop o) (init o) ⟨by simp [init], by simp [init], by simp [init],
    by simp [init]⟩
  exact ⟨this.1, this.2.1⟩

theorem read_keys_nodup (f : List Nat) (o : Nat) : ((read f o).1.map Prod.fst).Nodup := by
  have hp := (read_keys f o).1
  unfold List.Nodup
  rw [List.pairwise_map]
  exact hp.imp fun h => Nat.ne_of_lt h

/-! ## Reading from an earlier boundary -/

theorem read_full (xs : List Nat) (h : xs = [] ∨ ∃ ys', xs = ys' ++ [newline]) :
    (read xs 0).2 = xs.length := by
  have := (read_decomp xs 0 xs [] (by simp) (by simp) h).2.2
  rw [this]; simp

theorem take_boundary (f : List Nat) (o : Nat) (h : Boundary f o) :
    f.take o = [] ∨ ∃ ys', f.take o = ys' ++ [newline] := by
  rcases h with rfl | ⟨hp, hs⟩
  · simp
  · right
    refine ⟨f.take (o - 1), ?_⟩
    have : o = (o - 1) + 1 := by omega
    rw [this, List.take_add_one, hs]
    simp

/-- From a boundary `o`, a read returns exactly the suffix of the full read. -/
theorem read_from_boundary (f : List Nat) (o : Nat) (h : Boundary f o) :
    read f 0 = ((read (f.take o) 0).1 ++ (read f o).1, (read f o).2) := by
  have hle := h.le_length
  have hsplit := read_append (f.take o) (f.drop o) 0 (Nat.zero_le _)
  rw [List.take_append_drop] at hsplit
  have hn : (read (f.take o) 0).2 = o := by
    rw [read_full _ (take_boundary f o h)]
    simp; omega
  rw [hn] at hsplit
  exact hsplit

/-! ## 5. Client merge -/

section Merge
variable {α : Type}

/-- Insert by key, keeping the list sorted; an existing item wins on a clash. -/
def insertKey (x : Nat × α) : List (Nat × α) → List (Nat × α)
  | [] => [x]
  | y :: ys => if x.1 < y.1 then x :: y :: ys else if x.1 = y.1 then y :: ys else y :: insertKey x ys

/-- `ThreadModel::ingest`'s item merge. -/
def merge (xs new : List (Nat × α)) : List (Nat × α) :=
  new.foldl (fun acc x => insertKey x acc) xs

theorem insertKey_of_key_mem (x : Nat × α) (xs : List (Nat × α))
    (hm : ∃ y ∈ xs, y.1 = x.1) (hs : xs.Pairwise KeyLt) : insertKey x xs = xs := by
  induction xs with
  | nil => simp at hm
  | cons y ys ih =>
    obtain ⟨z, hz, hzx⟩ := hm
    rw [List.pairwise_cons] at hs
    simp only [insertKey]
    by_cases hlt : x.1 < y.1
    · exfalso
      rcases List.mem_cons.mp hz with rfl | hz
      · omega
      · have := hs.1 z hz; simp [KeyLt] at this; omega
    · by_cases heq : x.1 = y.1
      · simp [heq]
      · simp only [hlt, heq, ↓reduceIte]
        rcases List.mem_cons.mp hz with rfl | hz
        · exact absurd hzx.symm heq
        · rw [ih ⟨z, hz, hzx⟩ hs.2]

theorem insertKey_of_lt_all (x : Nat × α) (xs : List (Nat × α)) (h : ∀ y ∈ xs, y.1 < x.1) :
    insertKey x xs = xs ++ [x] := by
  induction xs with
  | nil => rfl
  | cons y ys ih =>
    have hy := h y List.mem_cons_self
    simp only [insertKey]
    have h1 : ¬ x.1 < y.1 := by omega
    have h2 : ¬ x.1 = y.1 := by omega
    simp only [h1, h2, ↓reduceIte, ih (fun z hz => h z (List.mem_cons_of_mem _ hz))]
    rfl

theorem mem_insertKey (x : Nat × α) (xs : List (Nat × α)) (z : Nat × α)
    (h : z ∈ insertKey x xs) : z = x ∨ z ∈ xs := by
  induction xs with
  | nil => simp [insertKey] at h; exact Or.inl h
  | cons y ys ih =>
    simp only [insertKey] at h
    split at h
    · simpa using h
    · split at h
      · exact Or.inr h
      · rcases List.mem_cons.mp h with rfl | h
        · exact Or.inr List.mem_cons_self
        · rcases ih h with e | e
          · exact Or.inl e
          · exact Or.inr (List.mem_cons_of_mem _ e)

theorem insertKey_sorted (x : Nat × α) (xs : List (Nat × α)) (hs : xs.Pairwise KeyLt) :
    (insertKey x xs).Pairwise KeyLt := by
  induction xs with
  | nil => simp [insertKey]
  | cons y ys ih =>
    rw [List.pairwise_cons] at hs
    simp only [insertKey]
    split
    · rename_i hlt
      refine List.Pairwise.cons ?_ (List.Pairwise.cons hs.1 hs.2)
      intro z hz
      rcases List.mem_cons.mp hz with rfl | hz
      · exact hlt
      · have := hs.1 z hz; simp only [KeyLt] at *; omega
    · split
      · exact List.Pairwise.cons hs.1 hs.2
      · rename_i hlt heq
        refine List.Pairwise.cons ?_ (ih hs.2)
        intro z hz
        rcases mem_insertKey x ys z hz with rfl | hz
        · simp only [KeyLt]; omega
        · exact hs.1 z hz

/-- Merge keeps the client's list sorted by key. -/
theorem merge_sorted (xs new : List (Nat × α)) (hs : xs.Pairwise KeyLt) :
    (merge xs new).Pairwise KeyLt := by
  induction new generalizing xs with
  | nil => exact hs
  | cons x rest ih => exact ih _ (insertKey_sorted x xs hs)

theorem merge_known (xs new : List (Nat × α)) (hs : xs.Pairwise KeyLt)
    (h : ∀ q ∈ new, ∃ y ∈ xs, y.1 = q.1) : merge xs new = xs := by
  induction new with
  | nil => rfl
  | cons q rest ih =>
    simp only [merge, List.foldl_cons]
    rw [insertKey_of_key_mem q xs (h q List.mem_cons_self) hs]
    exact ih (fun r hr => h r (List.mem_cons_of_mem _ hr))

theorem merge_fresh (held fresh : List (Nat × α)) (hs : (held ++ fresh).Pairwise KeyLt) :
    merge held fresh = held ++ fresh := by
  induction fresh generalizing held with
  | nil => simp [merge]
  | cons r rest ih =>
    simp only [merge, List.foldl_cons]
    have hlt : ∀ y ∈ held, y.1 < r.1 := fun y hy =>
      (List.pairwise_append.mp hs).2.2 y hy r List.mem_cons_self
    rw [insertKey_of_lt_all r held hlt]
    have := ih (held ++ [r]) (by simpa using hs)
    simpa [merge] using this

/-- Replaying items the client already holds, followed by newer items, appends exactly
    the newer items. -/
theorem merge_extend (held replayed fresh : List (Nat × α))
    (hs : (held ++ fresh).Pairwise KeyLt) (hr : ∀ q ∈ replayed, q ∈ held) :
    merge held (replayed ++ fresh) = held ++ fresh := by
  have hheld : held.Pairwise KeyLt := (List.pairwise_append.mp hs).1
  simp only [merge, List.foldl_append]
  have : replayed.foldl (fun acc x => insertKey x acc) held = held :=
    merge_known held replayed hheld (fun q hq => ⟨q, hr q hq, rfl⟩)
  rw [this]
  exact merge_fresh held fresh hs

theorem merge_self (xs : List (Nat × α)) (hs : xs.Pairwise KeyLt) : merge xs xs = xs := by
  simpa using merge_extend xs xs [] (by simpa using hs) (fun _ h => h)

end Merge

/-- The Engine accepts a cursor only at a record boundary; otherwise it restarts at 0. -/
def serve (f : List Nat) (o : Nat) : List Item × Nat :=
  read f (if isBoundary f o then o else 0)

/-- Reconnecting from any earlier cursor (saved, stale, or 0 after an Engine restart),
    or from garbage the Engine rejects, after the file grew by any `g`, neither
    duplicates nor drops a line: the client ends up with exactly one full read. -/
theorem replay_idempotent (f g : List Nat) (o' : Nat)
    (stale : o' ≤ (read f 0).2 ∨ isBoundary (f ++ g) o' = false) :
    merge (read f 0).1 (serve (f ++ g) o').1 = (read (f ++ g) 0).1 ∧
    (serve (f ++ g) o').2 = (read (f ++ g) 0).2 := by
  have hfull := read_append f g 0 (Nat.zero_le _)
  have hc : (read f 0).2 ≤ f.length := read_le_length f 0 (Nat.zero_le _)
  generalize ho : (if isBoundary (f ++ g) o' then o' else 0) = o₀
  have hb' : Boundary (f ++ g) o₀ ∧ o₀ ≤ (read f 0).2 := by
    subst ho
    split
    · rename_i hb
      refine ⟨(isBoundary_iff _ _).mp hb, ?_⟩
      rcases stale with s | s
      · exact s
      · rw [hb] at s; cases s
    · exact ⟨Or.inl rfl, Nat.zero_le _⟩
  have hb : Boundary f o₀ := by
    rcases hb'.1 with e | ⟨hp, hs⟩
    · exact Or.inl e
    · refine Or.inr ⟨hp, ?_⟩
      rw [List.getElem?_append_left (by omega)] at hs
      exact hs
  have hsuffix := read_from_boundary f o₀ hb
  have hnext : (read f o₀).2 = (read f 0).2 := by rw [hsuffix]
  have hsplit := read_append f g o₀ (by omega)
  rw [hnext] at hsplit
  simp only [serve, ho, hsplit]
  refine ⟨?_, by rw [hfull]⟩
  have hkeys := (read_keys (f ++ g) 0).1
  rw [hfull] at hkeys
  rw [hfull]
  apply merge_extend _ _ _ hkeys
  intro q hq
  rw [hsuffix]
  exact List.mem_append_right _ hq

/-! ## 6. Lens state -/

inductive Lens where
  | gui | tui
  deriving DecidableEq, Repr

def Lens.toggle : Lens → Lens
  | .gui => .tui
  | .tui => .gui

/-- The client's view of one thread. The PTY is not part of this state: `flip` cannot
    write to it. -/
structure Client where
  items : List Item
  cursor : Nat
  lens : Lens

def ingest (c : Client) (r : List Item × Nat) : Client :=
  { c with items := merge c.items r.1, cursor := r.2 }

def flip (c : Client) : Client := { c with lens := c.lens.toggle }

theorem flip_preserves (c : Client) : (flip c).items = c.items ∧ (flip c).cursor = c.cursor :=
  ⟨rfl, rfl⟩

theorem lens_flip_commutes (c : Client) (r : List Item × Nat) :
    ingest (flip c) r = flip (ingest c r) := rfl

inductive Event where
  | flip
  | ingest (r : List Item × Nat)

def apply (c : Client) : Event → Client
  | .flip => flip c
  | .ingest r => ingest c r

def reads : List Event → List (List Item × Nat)
  | [] => []
  | .flip :: es => reads es
  | .ingest r :: es => r :: reads es

theorem ingests_flip (rs : List (List Item × Nat)) (c : Client) :
    rs.foldl ingest (flip c) = flip (rs.foldl ingest c) := by
  induction rs generalizing c with
  | nil => rfl
  | cons r rs ih => simp only [List.foldl_cons, lens_flip_commutes, ih]

/-- Any interleaving of GUI⇄TUI flips with ingests yields the items and cursor of the
    ingests alone. -/
theorem flips_never_drift (es : List Event) (c : Client) :
    (es.foldl apply c).items = ((reads es).foldl ingest c).items ∧
    (es.foldl apply c).cursor = ((reads es).foldl ingest c).cursor := by
  induction es generalizing c with
  | nil => exact ⟨rfl, rfl⟩
  | cons e es ih =>
    cases e with
    | flip =>
      simp only [List.foldl_cons, apply, reads]
      rw [(ih (flip c)).1, (ih (flip c)).2, ingests_flip]
      exact ⟨rfl, rfl⟩
    | ingest r =>
      simp only [List.foldl_cons, apply, reads]
      exact ih (ingest c r)

/-! ## End to end: a synced client stays synced -/

/-- The client holds exactly one full read of the file it has seen. -/
def Synced (c : Client) (f : List Nat) : Prop :=
  c.items = (read f 0).1 ∧ c.cursor = (read f 0).2

theorem synced_fresh (lens : Lens) : Synced ⟨[], 0, lens⟩ [] := by
  simp [Synced, read, scan, init]

/-- After the file grows by `g`, reconnecting with the saved cursor, any older cursor,
    or 0 keeps the client synced with the grown file. -/
theorem synced_reconnect (c : Client) (f g : List Nat) (o' : Nat) (h : Synced c f)
    (stale : o' ≤ c.cursor) : Synced (ingest c (serve (f ++ g) o')) (f ++ g) := by
  obtain ⟨hi, hc⟩ := h
  have := replay_idempotent f g o' (Or.inl (hc ▸ stale))
  simp only [Synced, ingest, hi]
  exact this

theorem synced_flip (c : Client) (f : List Nat) (h : Synced c f) : Synced (flip c) f := h

end DualLens
