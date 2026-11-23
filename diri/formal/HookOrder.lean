import Std

namespace HookOrder

variable {S E : Type}

-- A cancelled reservation has no event. Sequence numbers need not be dense.
abbrev Ticket (E : Type) := Nat × Option E

def applyTicket (reduce : S → E → S) (s : S) (t : Ticket E) : S :=
  t.2.elim s (reduce s)

-- The issued list is fixed. Receipts may be repeated or arrive in any order.
-- Only the head handler can pass its predecessor and consume its reservation.
inductive Run (reduce : S → E → S) (initial : S) (issued : List (Ticket E)) :
    List Nat → List (Ticket E) → List (Ticket E) → S → Prop where
  | start : Run reduce initial issued [] [] issued initial
  | receive : Run reduce initial issued arrived done pending s →
      Run reduce initial issued (n :: arrived) done pending s
  | finish : Run reduce initial issued arrived done (t :: pending) s →
      t.1 ∈ arrived →
      Run reduce initial issued arrived (done ++ [t]) pending (applyTicket reduce s t)

theorem invariant (h : Run reduce initial issued arrived done pending s) :
    done ++ pending = issued ∧ s = done.foldl (applyTicket reduce) initial := by
  induction h with
  | start => simp
  | receive _ ih => exact ih
  | finish _ _ ih => exact ⟨by simpa using ih.1, by simp [ih.2]⟩

-- Includes all interleavings of receive and finish, for an arbitrary reducer.
theorem final_in_order (h : Run reduce initial issued arrived done [] s) :
    s = issued.foldl (applyTicket reduce) initial := by
  obtain ⟨hs, hr⟩ := invariant h
  rw [← hs, List.append_nil]
  exact hr

theorem at_most_once {issued : List (Ticket E)} (unique : (issued.map Prod.fst).Nodup)
    (h : Run reduce initial issued arrived done pending s) :
    (done.map Prod.fst).Nodup := by
  have both : ((done ++ pending).map Prod.fst).Nodup := (invariant h).1 ▸ unique
  rw [List.map_append] at both
  exact (List.nodup_append.mp both).1

-- Every fully delivered stream can drain, independently of receipt order.
theorem drain (h : Run reduce initial issued arrived done pending s)
    (ready : ∀ t ∈ pending, t.1 ∈ arrived) :
    ∃ finished result, Run reduce initial issued arrived finished [] result := by
  induction pending generalizing done s with
  | nil => exact ⟨_, _, h⟩
  | cons t tail ih =>
    exact ih (.finish h (ready t (by simp))) (fun u hu => ready u (by simp [hu]))

theorem receive_all (xs : List Nat) (h : Run reduce initial issued arrived done pending s) :
    Run reduce initial issued (xs.reverse ++ arrived) done pending s := by
  induction xs generalizing arrived with
  | nil => simpa using h
  | cons x xs ih => simpa using ih (Run.receive (n := x) h)

theorem permutation_drains_in_order {arrivals : List Nat} {issued : List (Ticket E)} (order : arrivals.Perm (issued.map Prod.fst))
    (reduce : S → E → S) (initial : S) :
    ∃ done, Run reduce initial issued arrivals.reverse done []
      (issued.foldl (applyTicket reduce) initial) := by
  have h := receive_all arrivals (Run.start (reduce := reduce) (initial := initial) (issued := issued))
  simp only [List.append_nil] at h
  obtain ⟨done, result, hd⟩ := drain h (fun t ht => by
    apply List.mem_reverse.mpr
    exact order.mem_iff.mpr (List.mem_map.mpr ⟨t, ht, rfl⟩))
  exact ⟨done, final_in_order hd ▸ hd⟩

end HookOrder
