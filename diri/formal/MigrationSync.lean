import Std

/-! Portable profile sync and Mac-to-Mac migration (`diri-portable`).
    Mirrors the design brief: an artifact allowlist with secret exclusion, field
    redaction, idempotent session merge keyed by id, cwd-only path remap, and
    write-if-absent placement of transcripts and notes. -/

namespace Diri.Portable

/-! ## Secret exclusion -/

/-- `diri_portable::classify::ArtifactClass`. Everything not allowlisted is excluded. -/
inductive ArtifactClass where
  -- portable profile
  | note | appSkill | launchRecipe | schedule | preference | initLua | projectEntry
  | manifestOverride
  -- migration only
  | sessionRecord | claudeTranscript | codexTranscript
  -- secret / runtime, never exported
  | credential | apiKey | accountToken | sshKey | browserCookie | unixSocket | holderPid
  | terminalSwap | accountProfile | hostConfig | remoteBinding | phoneToken | mcpHttpKey
  | telemetryState | runLog
  deriving DecidableEq, Repr

open ArtifactClass

def secret : ArtifactClass → Bool
  | credential | apiKey | accountToken | sshKey | browserCookie | unixSocket | holderPid
  | terminalSwap | accountProfile | hostConfig | remoteBinding | phoneToken | mcpHttpKey
  | telemetryState | runLog => true
  | _ => false

/-- `classify::profile_allowed`. -/
def profileAllowed : ArtifactClass → Bool
  | note | appSkill | launchRecipe | schedule | preference | initLua | projectEntry
  | manifestOverride => true
  | _ => false

/-- `classify::migration_allowed`: the profile plus sanitized sessions and transcripts. -/
def migrationAllowed : ArtifactClass → Bool
  | note | appSkill | launchRecipe | schedule | preference | initLua | projectEntry
  | manifestOverride | sessionRecord | claudeTranscript | codexTranscript => true
  | _ => false

theorem profile_allowed_not_secret (c : ArtifactClass) (h : profileAllowed c = true) :
    secret c = false := by cases c <;> simp_all [profileAllowed, secret]

theorem migration_allowed_not_secret (c : ArtifactClass) (h : migrationAllowed c = true) :
    secret c = false := by cases c <;> simp_all [migrationAllowed, secret]

theorem profile_subset_migration (c : ArtifactClass) (h : profileAllowed c = true) :
    migrationAllowed c = true := by cases c <;> simp_all [profileAllowed, migrationAllowed]

/-- A JSON document flattened to (key, value) pairs; nested keys and manifest `env`
    entries appear as their own pairs. -/
structure Artifact where
  cls : ArtifactClass
  fields : List (String × String)

/-- `classify::redact_secret_fields`. `secretKey` is arbitrary; Rust matches the
    lowercase key against token/secret/password/... (design brief). -/
def redact (secretKey : String → Bool) (a : Artifact) : Artifact :=
  { a with fields := a.fields.filter (fun f => !secretKey f.1) }

def exportWith (allowed : ArtifactClass → Bool) (secretKey : String → Bool)
    (xs : List Artifact) : List Artifact :=
  (xs.filter (allowed ·.cls)).map (redact secretKey)

def profileExport := exportWith profileAllowed
def migrationExport := exportWith migrationAllowed

theorem export_allowed {allowed k xs a} (h : a ∈ exportWith allowed k xs) :
    allowed a.cls = true := by
  simp only [exportWith, List.mem_map, List.mem_filter] at h
  obtain ⟨b, ⟨_, hb⟩, rfl⟩ := h
  exact hb

theorem profile_export_no_secret (k xs) (a : Artifact) (h : a ∈ profileExport k xs) :
    secret a.cls = false := profile_allowed_not_secret _ (export_allowed h)

theorem migration_export_no_secret (k xs) (a : Artifact) (h : a ∈ migrationExport k xs) :
    secret a.cls = false := migration_allowed_not_secret _ (export_allowed h)

theorem redact_no_secret_field (k : String → Bool) (a : Artifact) (f : String × String)
    (h : f ∈ (redact k a).fields) : k f.1 = false := by
  simp only [redact, List.mem_filter] at h
  simpa using h.2

theorem export_no_secret_field (allowed k xs) (a : Artifact) (f : String × String)
    (ha : a ∈ exportWith allowed k xs) (hf : f ∈ a.fields) : k f.1 = false := by
  simp only [exportWith, List.mem_map] at ha
  obtain ⟨b, _, rfl⟩ := ha
  exact redact_no_secret_field k b f hf

theorem redact_idempotent (k : String → Bool) (a : Artifact) :
    redact k (redact k a) = redact k a := by
  simp [redact, List.filter_filter]

/-- Exporting an export changes nothing (holds for the profile and migration allowlists). -/
theorem export_idempotent (allowed k) (xs : List Artifact) :
    exportWith allowed k (exportWith allowed k xs) = exportWith allowed k xs := by
  induction xs with
  | nil => rfl
  | cons a t ih =>
    by_cases h : allowed a.cls = true
    · simp_all [exportWith, redact]
    · simp_all [exportWith]

/-! ## Session merge -/

abbrev Path := List String

structure Rec where
  id : Nat
  parent : Option Nat
  updated : Nat
  conversation : Option String
  cwd : Path
  live : Bool

/-- Local session store keyed by id. -/
abbrev Store := Nat → Option Rec

/-- No duplicate ids: every stored record sits under its own id. -/
def WellKeyed (s : Store) : Prop := ∀ i r, s i = some r → r.id = i

/-- An imported record is never live (status exited{daemonRestart}). -/
def strip (r : Rec) : Rec := { r with live := false }

/-- Decision for one id: absent → add; local newer-or-equal or live → keep; else replace. -/
def pick (cur : Option Rec) (inc : Rec) : Option Rec :=
  match cur with
  | none => some (strip inc)
  | some loc => if inc.updated ≤ loc.updated ∨ loc.live = true then some loc else some (strip inc)

def mergeOne (s : Store) (inc : Rec) : Store :=
  fun i => if i = inc.id then pick (s i) inc else s i

/-- `diri_portable::sessions::merge_sessions`. -/
def merge (s : Store) (bundle : List Rec) : Store := bundle.foldl mergeOne s

theorem mergeOne_self (s : Store) (b : Rec) : mergeOne s b b.id = pick (s b.id) b := by
  simp [mergeOne]

theorem mergeOne_other (s : Store) (b : Rec) (i : Nat) (h : i ≠ b.id) :
    mergeOne s b i = s i := by simp [mergeOne, h]

theorem pick_keep (loc b : Rec) (h : b.updated ≤ loc.updated ∨ loc.live = true) :
    pick (some loc) b = some loc := by simp [pick, h]

theorem pick_replace (loc b : Rec) (h : ¬(b.updated ≤ loc.updated ∨ loc.live = true)) :
    pick (some loc) b = some (strip b) := by simp only [pick, h, ite_false]

/-- `s` already absorbs `b`: a record under its id that is live or at least as new. -/
def Covers (s : Store) (b : Rec) : Prop :=
  ∃ loc, s b.id = some loc ∧ (b.updated ≤ loc.updated ∨ loc.live = true)

theorem mergeOne_covered (s : Store) (b : Rec) (h : Covers s b) : mergeOne s b = s := by
  obtain ⟨loc, hs, hc⟩ := h
  funext i
  by_cases hi : i = b.id
  · subst hi; rw [mergeOne_self, hs, pick_keep _ _ hc]
  · exact mergeOne_other s b i hi

theorem mergeOne_covers (s : Store) (b : Rec) : Covers (mergeOne s b) b := by
  unfold Covers; rw [mergeOne_self]
  cases h : s b.id with
  | none => exact ⟨strip b, rfl, by simp [strip]⟩
  | some loc =>
    by_cases hc : b.updated ≤ loc.updated ∨ loc.live = true
    · exact ⟨loc, pick_keep _ _ hc, hc⟩
    · exact ⟨strip b, pick_replace _ _ hc, by simp [strip]⟩

theorem mergeOne_keeps_cover (s : Store) (b b' : Rec) (h : Covers s b') :
    Covers (mergeOne s b) b' := by
  obtain ⟨loc, hs, hc⟩ := h
  by_cases hi : b'.id = b.id
  · rw [Covers, hi, mergeOne_self, ← hi, hs]
    by_cases hk : b.updated ≤ loc.updated ∨ loc.live = true
    · exact ⟨loc, pick_keep _ _ hk, hc⟩
    · refine ⟨strip b, pick_replace _ _ hk, ?_⟩
      simp only [not_or] at hk
      cases hc with
      | inl h => left; simp [strip]; omega
      | inr h => exact absurd h hk.2
  · exact ⟨loc, by rw [mergeOne_other s b _ hi, hs], hc⟩

theorem merge_covers (s : Store) (B : List Rec) : ∀ b ∈ B, Covers (merge s B) b := by
  induction B generalizing s with
  | nil => simp
  | cons b t ih =>
    intro b' hb'
    simp only [merge, List.foldl_cons] at *
    rcases List.mem_cons.mp hb' with rfl | hm
    · -- covered after this step, and later steps keep the cover
      have := mergeOne_covers s b'
      generalize mergeOne s b' = s' at this
      clear ih hb'
      induction t generalizing s' with
      | nil => exact this
      | cons c t ih2 => exact ih2 _ (mergeOne_keeps_cover _ _ _ this)
    · exact ih _ b' hm

theorem merge_of_covers (s : Store) (B : List Rec) (h : ∀ b ∈ B, Covers s b) :
    merge s B = s := by
  induction B with
  | nil => rfl
  | cons b t ih =>
    simp only [merge, List.foldl_cons, mergeOne_covered s b (h b (by simp))]
    exact ih (fun c hc => h c (by simp [hc]))

/-- Importing the same bundle twice changes nothing the second time. -/
theorem merge_idempotent (s : Store) (B : List Rec) : merge (merge s B) B = merge s B :=
  merge_of_covers _ _ (merge_covers s B)

/-- One step keeps every local id and never lowers its `updated`; a live record is kept. -/
theorem mergeOne_monotone (s : Store) (b : Rec) (i : Nat) (loc : Rec) (h : s i = some loc) :
    ∃ r, mergeOne s b i = some r ∧ loc.updated ≤ r.updated ∧ (loc.live = true → r = loc) := by
  by_cases hi : i = b.id
  · subst hi; rw [mergeOne_self, h]
    by_cases hk : b.updated ≤ loc.updated ∨ loc.live = true
    · exact ⟨loc, pick_keep _ _ hk, Nat.le_refl _, fun _ => rfl⟩
    · simp only [not_or] at hk
      refine ⟨strip b, pick_replace _ _ (by simp [hk]), by simp [strip]; omega, ?_⟩
      intro hl; exact absurd hl hk.2
  · exact ⟨loc, by rw [mergeOne_other s b i hi, h], Nat.le_refl _, fun _ => rfl⟩

/-- Merge never removes a local id and never lowers its `updated`. -/
theorem merge_monotone (s : Store) (B : List Rec) (i : Nat) (loc : Rec) (h : s i = some loc) :
    ∃ r, merge s B i = some r ∧ loc.updated ≤ r.updated := by
  induction B generalizing s loc with
  | nil => exact ⟨loc, h, Nat.le_refl _⟩
  | cons b t ih =>
    obtain ⟨r, hr, hle, _⟩ := mergeOne_monotone s b i loc h
    obtain ⟨r', hr', hle'⟩ := ih _ r hr
    exact ⟨r', hr', Nat.le_trans hle hle'⟩

/-- A live local record is untouched by any bundle. -/
theorem merge_keeps_live (s : Store) (B : List Rec) (i : Nat) (loc : Rec)
    (h : s i = some loc) (hl : loc.live = true) : merge s B i = some loc := by
  induction B generalizing s with
  | nil => exact h
  | cons b t ih =>
    obtain ⟨r, hr, _, he⟩ := mergeOne_monotone s b i loc h
    exact ih _ (by rw [hr, he hl])

/-- Every record `pick` can return is the current one or the stripped incoming one. -/
theorem pick_cases (cur : Option Rec) (b r : Rec) (h : pick cur b = some r) :
    cur = some r ∨ r = strip b := by
  cases cur with
  | none => simp [pick] at h; exact .inr h.symm
  | some loc =>
    by_cases hk : b.updated ≤ loc.updated ∨ loc.live = true
    · rw [pick_keep _ _ hk] at h; exact .inl h
    · rw [pick_replace _ _ hk] at h; exact .inr (Option.some.inj h).symm

/-- No duplicate ids: a well-keyed store stays well keyed. -/
theorem merge_wellKeyed (s : Store) (B : List Rec) (h : WellKeyed s) :
    WellKeyed (merge s B) := by
  induction B generalizing s with
  | nil => exact h
  | cons b t ih =>
    apply ih
    intro i r hr
    by_cases hi : i = b.id
    · subst hi
      rw [mergeOne_self] at hr
      rcases pick_cases _ _ _ hr with h' | rfl
      · exact h _ _ h'
      · rfl
    · rw [mergeOne_other s b i hi] at hr; exact h i r hr

/-- Imported records are never live unless they were already local and live. -/
theorem merge_imports_not_live (s : Store) (B : List Rec) (i : Nat) (r : Rec)
    (hs : ∀ loc, s i = some loc → loc.live = false) (h : merge s B i = some r) :
    r.live = false := by
  induction B generalizing s with
  | nil => exact hs r h
  | cons b t ih =>
    refine ih _ ?_ h
    intro loc hl
    by_cases hi : i = b.id
    · subst hi
      rw [mergeOne_self] at hl
      rcases pick_cases _ _ _ hl with h' | rfl
      · exact hs _ h'
      · rfl
    · rw [mergeOne_other s b i hi] at hl; exact hs loc hl

/-! ## Remap -/

/-- `diri_portable::pathmap::PathMap::remap` applied to a session: only `cwd` changes. -/
def remap (f : Path → Path) (r : Rec) : Rec := { r with cwd := f r.cwd }

theorem remap_preserves (f : Path → Path) (r : Rec) :
    (remap f r).id = r.id ∧ (remap f r).parent = r.parent ∧
    (remap f r).updated = r.updated ∧ (remap f r).conversation = r.conversation ∧
    (remap f r).live = r.live := by simp [remap]

def resumable (r : Rec) : Bool := r.conversation.isSome

theorem remap_resumable (f : Path → Path) (r : Rec) : resumable (remap f r) = resumable r := rfl

/-- Parent edges (parent, child) of a bundle. -/
def lineage (B : List Rec) : List (Nat × Nat) :=
  B.filterMap (fun r => r.parent.map (fun p => (p, r.id)))

theorem remap_lineage (f : Path → Path) (B : List Rec) :
    lineage (B.map (remap f)) = lineage B := by
  simp [lineage, List.filterMap_map, Function.comp_def, remap]

theorem merge_remap_idempotent (f : Path → Path) (s : Store) (B : List Rec) :
    merge (merge s (B.map (remap f))) (B.map (remap f)) = merge s (B.map (remap f)) :=
  merge_idempotent _ _

/-- One `OLD=NEW` rule: replace a leading `old`, otherwise leave the path alone. -/
def remapPrefix (old new : Path) (p : Path) : Path :=
  if old.isPrefixOf p then new ++ p.drop old.length else p

theorem remapPrefix_hit (old new s : Path) : remapPrefix old new (old ++ s) = new ++ s := by
  have : old.isPrefixOf (old ++ s) = true := by simp [List.isPrefixOf_iff_prefix]
  simp [remapPrefix, this]

theorem remapPrefix_miss (old new p : Path) (h : old.isPrefixOf p = false) :
    remapPrefix old new p = p := by simp [remapPrefix, h]

/-! ## Write-if-absent (transcripts, notes, skills, overrides) -/

abbrev Files (α : Type) := Path → Option α

/-- `diri_portable::files::write_if_absent`. -/
def writeIfAbsent {α} (fs : Files α) (p : Path) (c : α) : Files α :=
  fun q => if q = p ∧ fs p = none then some c else fs q

theorem writeIfAbsent_keeps {α} (fs : Files α) (p q : Path) (c x : α) (h : fs q = some x) :
    writeIfAbsent fs p c q = some x := by
  unfold writeIfAbsent
  by_cases hq : q = p
  · subst hq; simp [h]
  · simp [hq, h]

/-- A second write at the same path, with any content, changes nothing. -/
theorem writeIfAbsent_idempotent {α} (fs : Files α) (p : Path) (c c' : α) :
    writeIfAbsent (writeIfAbsent fs p c) p c' = writeIfAbsent fs p c := by
  funext q
  have hp : writeIfAbsent fs p c p ≠ none := by
    unfold writeIfAbsent; cases h : fs p <;> simp
  simp only [writeIfAbsent] at hp ⊢
  by_cases hq : q = p
  · subst hq; simp only [true_and] at hp ⊢; simp [hp]
  · simp [hq]

end Diri.Portable
