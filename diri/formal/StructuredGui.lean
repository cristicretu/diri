import Std

namespace Diri.StructuredGui

/-! Model of `crates/diri-engine/src/structured.rs` (`StructuredConversation`).
    It checks the invariants the Rust tests exercise, not the Agent protocols:
    Claude's and Codex's wire formats are checked against recorded fixtures in
    `diri-proto`. Names follow the Rust methods: `poll` ingests, `switch_harness`,
    `set_model`, `send_turn`, and the terminal lease for the PTY attach. -/

/-! ### Monotonic event ingestion -/

structure Feed where
  last : Nat := 0
  deriving Repr

/-- `push` in `StructuredConversation`: each emitted event takes the next sequence. -/
def emit (f : Feed) : Nat × Feed :=
  (f.last + 1, { last := f.last + 1 })

theorem emit_sequence_exceeds_previous (f : Feed) :
    (emit f).1 > f.last := by
  simp [emit]

/-- The sequences of a run of `n` events are exactly `last+1 … last+n`. -/
def emitAll : Feed → Nat → List Nat
  | _, 0 => []
  | f, n + 1 => (emit f).1 :: emitAll (emit f).2 n

theorem emitAll_eq_range (f : Feed) (n : Nat) :
    emitAll f n = List.range' (f.last + 1) n := by
  induction n generalizing f with
  | zero => rfl
  | succ n ih =>
    rw [List.range'_succ]
    simp only [emitAll, emit]
    rw [ih]

/-- Every event in a run gets a sequence strictly greater than the previous one. -/
theorem emitAll_strictly_increasing (f : Feed) (n : Nat) :
    (emitAll f n).Pairwise (· < ·) := by
  rw [emitAll_eq_range]
  exact List.pairwise_lt_range'

/-! ### Harness switch keeps one conversation -/

structure Conversation where
  harness : Nat
  thread : Option Nat
  dividers : Nat
  deriving Repr

/-- `switch_harness` drops the native thread and records one divider; the
    conversation identity (the `Conversation` value) is unchanged. -/
def switchHarness (c : Conversation) (h : Nat) : Conversation :=
  if h = c.harness then c
  else { harness := h, thread := none, dividers := c.dividers + 1 }

theorem switch_same_harness_is_identity (c : Conversation) :
    switchHarness c c.harness = c := by
  simp [switchHarness]

theorem switch_adds_exactly_one_divider (c : Conversation) (h : Nat) (hne : h ≠ c.harness) :
    (switchHarness c h).dividers = c.dividers + 1 := by
  simp [switchHarness, hne]

/-! ### No Agent process before the first turn -/

inductive Op where
  | setModel
  | switchHarness
  | sendTurn
  deriving DecidableEq, Repr

structure Proc where
  spawns : Nat := 0
  hasWorker : Bool := false
  deriving Repr

/-- `send_turn` starts a worker only when none is running. `set_model` and
    `switch_harness` never spawn. -/
def apply (p : Proc) : Op → Proc
  | .setModel => p
  | .switchHarness => { p with hasWorker := false }
  | .sendTurn =>
    if p.hasWorker then p else { p with spawns := p.spawns + 1, hasWorker := true }

def run (p : Proc) : List Op → Proc
  | [] => p
  | o :: os => run (apply p o) os

/-- Choosing or switching models before the first turn costs no process. -/
theorem no_spawn_without_turn (p : Proc) (ops : List Op)
    (h : ∀ o ∈ ops, o ≠ Op.sendTurn) :
    (run p ops).spawns = p.spawns := by
  induction ops generalizing p with
  | nil => rfl
  | cons o os ih =>
    have ho : o ≠ Op.sendTurn := h o (by simp)
    have hos : ∀ x ∈ os, x ≠ Op.sendTurn := fun x hx => h x (by simp [hx])
    cases o with
    | setModel => exact ih p hos
    | switchHarness => exact ih { p with hasWorker := false } hos
    | sendTurn => exact absurd rfl ho

theorem fresh_conversation_spawns_nothing (ops : List Op)
    (h : ∀ o ∈ ops, o ≠ Op.sendTurn) :
    (run {} ops).spawns = 0 :=
  no_spawn_without_turn {} ops h

/-! ### Structured <-> PTY lease exclusivity -/

inductive Owner where
  | none
  | structured
  | pty
  deriving DecidableEq, Repr

/-- Attaching the PTY on a conversation takes the lease only when it is free. -/
def acquire (cur target : Owner) : Owner :=
  if cur = .none then target else cur

/-- A held lease is never taken over: a second writer cannot appear. -/
theorem held_lease_not_taken (cur target : Owner) (h : cur ≠ .none) :
    acquire cur target = cur := by
  simp [acquire, h]

/-- The PTY cannot take a conversation from the structured driver, and the
    structured driver cannot take it from the PTY: the holder keeps it. -/
theorem structured_blocks_pty : acquire .structured .pty = .structured := by
  simp [acquire]

theorem pty_blocks_structured : acquire .pty .structured = .pty := by
  simp [acquire]

theorem lease_released_then_reacquired (target : Owner) :
    acquire .none target = target := by
  simp [acquire]

end Diri.StructuredGui
