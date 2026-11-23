import Std

namespace OnboardingImport

-- One past conversation found on disk. `scratch` = cwd under /tmp, /private/tmp or a scratchpad.
structure Candidate where
  id : String
  lastActive : Nat
  subagent : Bool
  empty : Bool
  scratch : Bool
  cwdExists : Bool
  deriving DecidableEq

def junk (c : Candidate) : Bool := c.subagent || c.empty || c.scratch || !c.cwdExists

-- `tracked` = conversation ids already in Diri's registry.
def eligible (tracked : List String) (c : Candidate) : Bool :=
  !junk c && decide (c.id ∉ tracked)

-- Keeps the first occurrence of each conversation id.
def dedup : List Candidate → List Candidate
  | [] => []
  | c :: cs => c :: (dedup cs).filter (fun d => d.id != c.id)

def newer (a b : Candidate) : Bool := decide (b.lastActive ≤ a.lastActive)

def discover (tracked : List String) (cs : List Candidate) : List Candidate :=
  ((dedup cs).filter (eligible tracked)).mergeSort newer

-- The default selection and the browse list.
def suggested (tracked : List String) (cs : List Candidate) := (discover tracked cs).take 5
def more (tracked : List String) (cs : List Candidate) := (discover tracked cs).drop 5

theorem mem_dedup {x : Candidate} : ∀ {cs : List Candidate}, x ∈ dedup cs → x ∈ cs
  | [], h => by simp [dedup] at h
  | c :: cs, h => by
    simp only [dedup, List.mem_cons, List.mem_filter] at h
    rcases h with h | ⟨h, _⟩
    · simp [h]
    · exact List.mem_cons_of_mem _ (mem_dedup h)

theorem dedup_nodup : ∀ cs : List Candidate, ((dedup cs).map Candidate.id).Nodup
  | [] => by simp [dedup]
  | c :: cs => by
    simp only [dedup, List.map_cons, List.nodup_cons]
    refine ⟨by simp, ?_⟩
    exact (dedup_nodup cs).sublist (List.filter_sublist.map _)

theorem mem_discover {tracked cs x} :
    x ∈ discover tracked cs ↔ x ∈ dedup cs ∧ eligible tracked x = true := by
  simp [discover]

/-! ## 1. Default selection -/

theorem discover_sorted (tracked cs) :
    (discover tracked cs).Pairwise (fun a b => b.lastActive ≤ a.lastActive) := by
  have h := List.pairwise_mergeSort (le := newer)
    (fun a b c hab hbc => by simp [newer] at *; omega)
    (fun a b => by simp [newer]; omega) ((dedup cs).filter (eligible tracked))
  exact h.imp (by simp [newer])

-- Each conversation appears at most once.
theorem discover_ids_nodup (tracked cs) : ((discover tracked cs).map Candidate.id).Nodup :=
  ((List.mergeSort_perm _ _).map _).nodup_iff.mpr
    ((dedup_nodup cs).sublist (List.filter_sublist.map _))

theorem suggested_le_five (tracked cs) : (suggested tracked cs).length ≤ 5 :=
  List.length_take_le _ _

theorem suggested_sorted (tracked cs) :
    (suggested tracked cs).Pairwise (fun a b => b.lastActive ≤ a.lastActive) :=
  (discover_sorted tracked cs).sublist (List.take_sublist _ _)

-- Browse exposes exactly the remainder.
theorem suggested_append_more (tracked cs) :
    suggested tracked cs ++ more tracked cs = discover tracked cs :=
  List.take_append_drop _ _

/-! ## 3a. Filter -/

theorem discover_filtered {tracked cs c} (h : c ∈ discover tracked cs) :
    junk c = false ∧ c.id ∉ tracked ∧ c ∈ cs := by
  obtain ⟨hd, he⟩ := mem_discover.mp h
  simp [eligible] at he
  exact ⟨he.1, he.2, mem_dedup hd⟩

/-! ## Import -/

structure Record where
  agentId : String
  running : Bool
  resumable : Bool
  deriving DecidableEq

-- An imported record is a stopped, resumable entry: nothing spawns.
def fresh (c : Candidate) : Record := { agentId := c.id, running := false, resumable := true }

def ids (reg : List Record) : List String := reg.map Record.agentId

def importOne (reg : List Record) (c : Candidate) : List Record :=
  if eligible (ids reg) c then reg ++ [fresh c] else reg

def importAll (reg : List Record) (cs : List Candidate) : List Record := cs.foldl importOne reg

theorem importAll_cons (reg c cs) : importAll reg (c :: cs) = importAll (importOne reg c) cs := rfl

-- Import only appends, and each appended record is a fresh record for a non-junk candidate.
theorem importAll_appends : ∀ (reg : List Record) (cs : List Candidate),
    ∃ added, importAll reg cs = reg ++ added ∧
      ∀ r ∈ added, ∃ c ∈ cs, junk c = false ∧ r = fresh c
  | reg, [] => ⟨[], by simp [importAll]⟩
  | reg, c :: cs => by
    obtain ⟨a0, h0, s0⟩ : ∃ a0, importOne reg c = reg ++ a0 ∧
        ∀ r ∈ a0, junk c = false ∧ r = fresh c := by
      unfold importOne
      split
      · rename_i he
        simp [eligible] at he
        exact ⟨[fresh c], rfl, by simp [he.1]⟩
      · exact ⟨[], by simp⟩
    obtain ⟨a1, h1, s1⟩ := importAll_appends (importOne reg c) cs
    refine ⟨a0 ++ a1, by rw [importAll_cons, h1, h0, List.append_assoc], ?_⟩
    intro r hr
    rcases List.mem_append.mp hr with hr | hr
    · exact ⟨c, by simp, s0 r hr⟩
    · obtain ⟨d, hd, rest⟩ := s1 r hr
      exact ⟨d, List.mem_cons_of_mem _ hd, rest⟩

/-! ## 2. Idempotent import -/

theorem importAll_prefix (reg cs) : reg <+: importAll reg cs := by
  obtain ⟨added, h, _⟩ := importAll_appends reg cs
  exact ⟨added, h.symm⟩

theorem importOne_nodup {reg : List Record} (c) (h : (ids reg).Nodup) :
    (ids (importOne reg c)).Nodup := by
  unfold importOne
  split
  · rename_i he
    simp [eligible] at he
    simp only [ids, List.map_append, List.nodup_append]
    refine ⟨h, by simp, ?_⟩
    simp only [fresh, List.map_cons, List.map_nil, List.mem_singleton]
    intro a ha b hb hab
    exact he.2 (hb ▸ hab ▸ ha)
  · exact h

theorem importAll_nodup : ∀ {reg : List Record} (cs : List Candidate),
    (ids reg).Nodup → (ids (importAll reg cs)).Nodup
  | _, [], h => h
  | _, c :: cs, h => importAll_nodup cs (importOne_nodup c h)

theorem ids_grow (reg cs) : ∀ x ∈ ids reg, x ∈ ids (importAll reg cs) := by
  obtain ⟨added, h, _⟩ := importAll_appends reg cs
  intro x hx
  simp only [h, ids, List.map_append, List.mem_append]
  exact Or.inl hx

theorem ineligible_mono {t t' : List String} {c : Candidate}
    (h : eligible t c = false) (sub : ∀ x ∈ t, x ∈ t') : eligible t' c = false := by
  simp only [eligible, Bool.and_eq_false_iff, Bool.not_eq_false', decide_eq_false_iff_not,
    Classical.not_not] at *
  rcases h with h | h
  · exact Or.inl h
  · exact Or.inr (sub _ h)

theorem importOne_covers (reg c) : eligible (ids (importOne reg c)) c = false := by
  unfold importOne
  split
  · simp [eligible, ids, fresh]
  · rename_i he
    simpa using he

-- After a batch, no candidate of that batch is eligible any more.
theorem importAll_covers : ∀ (reg : List Record) (cs : List Candidate),
    ∀ c ∈ cs, eligible (ids (importAll reg cs)) c = false
  | _, [] => by simp
  | reg, d :: cs => by
    intro c hc
    rw [importAll_cons]
    rcases List.mem_cons.mp hc with rfl | hc
    · exact ineligible_mono (importOne_covers reg c) (ids_grow _ cs)
    · exact importAll_covers _ cs c hc

theorem importAll_noop : ∀ (reg : List Record) (ys : List Candidate),
    (∀ y ∈ ys, eligible (ids reg) y = false) → importAll reg ys = reg
  | _, [], _ => rfl
  | reg, y :: ys, h => by
    rw [importAll_cons]
    have : importOne reg y = reg := by simp [importOne, h y (by simp)]
    rw [this]
    exact importAll_noop reg ys (fun z hz => h z (List.mem_cons_of_mem _ hz))

-- Re-importing any part of the same batch adds nothing.
theorem reimport_subset (reg cs ys) (sub : ∀ y ∈ ys, y ∈ cs) :
    importAll (importAll reg cs) ys = importAll reg cs :=
  importAll_noop _ ys (fun y hy => importAll_covers reg cs y (sub y hy))

theorem reimport (reg cs) : importAll (importAll reg cs) cs = importAll reg cs :=
  reimport_subset reg cs cs (fun _ h => h)

/-! ## 3b. Zero-spawn safety -/

theorem added_safe {reg cs added} (h : importAll reg cs = reg ++ added) :
    ∀ r ∈ added, r.running = false ∧ r.resumable = true ∧
      ∃ c ∈ cs, junk c = false ∧ r.agentId = c.id := by
  obtain ⟨a, ha, sa⟩ := importAll_appends reg cs
  have : added = a := List.append_cancel_left (h.symm.trans ha)
  subst this
  intro r hr
  obtain ⟨c, hc, hj, rfl⟩ := sa r hr
  exact ⟨rfl, rfl, c, hc, hj, rfl⟩

end OnboardingImport
