import Reliability
open Diri

-- Each attempt is an atomic caller action, split into the same internal phases as deliver_at.
-- Other callers can interleave reservation attempts; only the fresh caller runs its continuation.
inductive Attempt where | sent | rejected | crashBefore | crashAfter | uncertain
  deriving Repr

def events : Attempt → List DeliveryEvent
  | .sent => [.reserve, .effect, .commit]
  | .rejected => [.reserve, .noEffect]
  | .crashBefore => [.reserve, .engineRestart]
  | .crashAfter | .uncertain => [.reserve, .effect, .engineRestart]

def attempts := [Attempt.sent, .rejected, .crashBefore, .crashAfter, .uncertain]

def label : Attempt → String
  | .sent => "sent"
  | .rejected => "rejected"
  | .crashBefore => "crash-before"
  | .crashAfter => "crash-after"
  | .uncertain => "uncertain"

def attempt (s : Entry) (a : Attempt) : Entry :=
  if s.phase = .empty then (events a).foldl advance s else s

def outcome (s : Entry) : String :=
  match s.phase with
  | .empty => "absent"
  | .confirmed => "confirmed"
  | _ => "unknown"

def main : IO Unit := do
  for a in attempts do
    for b in attempts do
      let first := attempt {} a
      let afterRestart := advance (advance first .appRestart) .engineRestart
      let last := attempt afterRestart b
      IO.println (String.intercalate "\t" [label a, label b, outcome last, toString last.effects])
