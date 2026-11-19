import Std

namespace Diri

/-- Projection onto one arbitrary (sender, target, message_id), or (sender, operation_id).
    Message and operation ledgers are disjoint. Rows in all three intermediate phases
    have durable outcome "unknown"; only the original invocation owns the right to act. -/
inductive Phase where
  | empty | reserved | acted | unknown | confirmed
  deriving DecidableEq, Repr

structure Entry where
  phase : Phase := .empty
  effects : Nat := 0 -- ghost history: calls that could have affected the target
  deriving Repr

inductive DeliveryEvent where
  | reserve | noEffect | effect | commit | engineRestart | appRestart
  deriving Repr

/-- control::message_delivery::deliver_at and operations::{reserve,finish}.
    An ineligible event (including a duplicate reservation) cannot acquire ownership.
    A before-effect release is possible only for the owner, before any effect. -/
def advance (s : Entry) (event : DeliveryEvent) : Entry :=
  match event, s.phase with
  | .reserve, .empty => { s with phase := .reserved }
  | .noEffect, .reserved => { s with phase := .empty }
  | .effect, .reserved => ⟨.acted, s.effects + 1⟩
  | .commit, .acted => { s with phase := .confirmed }
  | .engineRestart, .reserved | .engineRestart, .acted => { s with phase := .unknown }
  | _, _ => s

def Consistent (s : Entry) : Prop :=
  match s.phase with
  | .empty | .reserved => s.effects = 0
  | .acted | .confirmed => s.effects = 1
  | .unknown => s.effects ≤ 1

theorem consistent_step (s : Entry) (e : DeliveryEvent) (h : Consistent s) :
    Consistent (advance s e) := by
  obtain ⟨p, n⟩ := s
  cases p <;> cases e <;> simp_all [Consistent, advance]

theorem consistent_trace (s : Entry) (events : List DeliveryEvent) (h : Consistent s) :
    Consistent (events.foldl advance s) := by
  induction events generalizing s with
  | nil => exact h
  | cons e es ih => exact ih _ (consistent_step _ _ h)

theorem at_most_once (events : List DeliveryEvent) :
    (events.foldl advance {}).effects ≤ 1 := by
  have h := consistent_trace {} events (by rfl)
  generalize events.foldl advance {} = out at *
  obtain ⟨p, n⟩ := out
  cases p <;> simp_all [Consistent]

theorem confirmed_persists (s : Entry) (events : List DeliveryEvent)
    (h : s.phase = .confirmed) : events.foldl advance s = s := by
  induction events with
  | nil => rfl
  | cons e es ih =>
    have step : advance s e = s := by cases e <;> simp [advance, h]
    simpa [List.foldl_cons, step] using ih

/-- The receipt of a successful input write is recoverable with its one historical
    effect, including after app restart followed by more requests or Engine crashes. -/
theorem sent_survives_app_restart (before after : List DeliveryEvent)
    (ack : (before.foldl advance {}).phase = .confirmed) :
    let reopened := (.appRestart :: after).foldl advance (before.foldl advance {})
    reopened.phase = .confirmed ∧ reopened.effects = 1 := by
  have hc := consistent_trace {} before (by rfl)
  rw [confirmed_persists _ _ ack]
  exact ⟨ack, by simpa [Consistent, ack] using hc⟩

/-- At expiry reserved_count can delete a receipt. Effects already performed survive
    the deletion. This executable witness marks the end of the identity guarantee. -/
def expire (s : Entry) : Entry := { s with phase := .empty }
private def sendOnce := ([.reserve, .effect, .commit] : List DeliveryEvent)
example : (sendOnce.foldl advance (expire (sendOnce.foldl advance {}))).effects = 2 := by decide

/-- Project tasks::reserve and tasks::report onto one task. Blocked and subsequent
    running/terminal states all retain the fact that the assignee acknowledged.
    Sender cancellation before acknowledgement is possible. -/
inductive TaskEvent where | submit | acknowledge | complete | cancel | appRestart | engineRestart
structure Task where
  present : Bool := false
  acknowledged : Bool := false
  terminal : Bool := false
  deriving DecidableEq

def taskStep (s : Task) (e : TaskEvent) : Task :=
  match e with
  | .submit => { s with present := true }
  | .acknowledge => if s.present && !s.terminal then { s with acknowledged := true } else s
  | .complete => if s.acknowledged && !s.terminal then { s with terminal := true } else s
  | .cancel => if s.present then { s with terminal := true } else s
  | .appRestart | .engineRestart => s

theorem task_ack_survives (s : Task) (es : List TaskEvent) (h : s.acknowledged = true) :
    (es.foldl taskStep s).acknowledged = true := by
  induction es generalizing s with
  | nil => exact h
  | cons e es ih =>
    apply ih
    cases e <;> simp only [taskStep] <;> (try split) <;> simp_all

theorem task_terminal_stable (s : Task) (e : TaskEvent)
    (present : s.present = true) (terminal : s.terminal = true) : taskStep s e = s := by
  obtain ⟨p, a, t⟩ := s
  cases e <;> simp_all [taskStep]

/-- A single PTY incarnation. The Holder owns process truth while the Engine
    reconstructs its view and the app reconnects. A negative observation is
    evidence from that incarnation, e.g. stat.alive=false, not a timeout. -/
inductive View where | unknown | running | exited
  deriving DecidableEq
structure Session where
  alive : Bool
  view : View
  deriving DecidableEq
inductive Observation where | appRestart | engineRestart | probeFailure | stat | exit

def observe (s : Session) : Observation → Session
  | .appRestart => s
  | .engineRestart | .probeFailure => { s with view := .unknown }
  | .stat => { s with view := if s.alive then .running else .exited }
  | .exit => ⟨false, .exited⟩

def Sound (s : Session) := s.alive = true → s.view ≠ .exited

theorem live_holder_never_reported_dead (s : Session) (es : List Observation)
    (h : Sound s) : Sound (es.foldl observe s) := by
  induction es generalizing s with
  | nil => exact h
  | cons e es ih =>
    apply ih
    cases e <;> simp_all [observe, Sound]

/-- Behavior at baseline 323a10ea: Registry::adopt_live_holders skips a failed stat;
    reap_orphans then marks that local session exited. The integration test
    ambiguous_stat_during_restore_cannot_end_a_surviving_holder supplies the witness. -/
def baselineRestoreOnFailedStat (s : Session) : Session := { s with view := .exited }

example : ¬ Sound (baselineRestoreOnFailedStat ⟨true, .running⟩) := by
  simp [Sound, baselineRestoreOnFailedStat]

/-- Holder::handshake, Connection::epoch, Holder::handle_message.
    checked_add fails admission on exhaustion; a failed attach leaves the lease intact. -/
def attach (epoch : Nat) : Option Nat :=
  if epoch < 2^64 - 1 then some (epoch + 1) else none

def accepts (current presented : Nat) : Prop := presented ≠ 0 ∧ presented = current

theorem attach_revokes (old next : Nat) (h : attach old = some next) :
    old < next ∧ accepts next next ∧ ¬ accepts next old := by
  unfold attach at h
  split at h <;> simp_all [accepts] <;> omega

theorem stale_rejected (current presented : Nat) (h : presented < current) :
    ¬ accepts current presented := by simp [accepts, Nat.ne_of_lt h]

end Diri
