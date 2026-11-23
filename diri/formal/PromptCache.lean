import Std

namespace Diri.Claude

structure Manifest where
  hooks : Bool
  mcp : Bool

/-- Effective preferences for this launch, including the per-tab voice selection. -/
structure Prefs where
  naming : Bool
  readsAloud : Bool

/-- Static resource bytes in catalog order (skills::ALL and the MCP tool list). -/
structure Project where
  instructions : String
  namingInstruction : String
  spokenInstruction : String
  skillFiles : List String
  toolSchemas : List String

structure Session where
  uuid : String
  token : String
  recoveryDir : String

structure Prompt where
  tools : List String
  instructions : List String
  skills : List String
  hookOutput : List String

structure Launch where
  visible : Prompt
  runtime : Session

/-- Model of injection_args_full, MCP initialize/tools/list, and prompt_hook_context.
    conversation_plan and name_session supply the runtime arguments and environment. -/
def inject (m : Manifest) (p : Prefs) (project : Project) (s : Session) : Launch :=
  { runtime := s
    visible := {
      tools := if m.mcp then project.toolSchemas else []
      instructions := if m.mcp then
        [project.instructions] ++ (if p.naming then [project.namingInstruction] else [])
        else []
      skills := if m.mcp then project.skillFiles else []
      hookOutput := if m.hooks && p.readsAloud then [project.spokenInstruction] else []
    } }

theorem session_independent (m p project) (a b : Session) :
    (inject m p project a).visible = (inject m p project b).visible := rfl

/-- MCP discovery and write_claude_skills_plugin preserve their static catalog order. -/
theorem catalog_order (m p project s) (enabled : m.mcp = true) :
    (inject m p project s).visible.tools = project.toolSchemas ∧
    (inject m p project s).visible.skills = project.skillFiles := by
  simp [inject, enabled]

/-! ## Dual-Lens observation metadata

The Engine records a thread's segment chain and the pane's lens for the GUI. None of it
is an input to injection. -/

inductive Lens where
  | gui | tui
  deriving DecidableEq, Repr

/-- One segment of a thread: which Agent served it, on which model and account, and the
    transcript offset where it starts. -/
structure Segment where
  agent : String
  model : String
  account : String
  start : Nat

structure Observation where
  segments : List Segment
  lens : Lens
  cursor : Nat

/-- A launch made while the Engine holds observation metadata. -/
structure ObservedLaunch where
  launch : Launch
  observation : Observation

def launchWith (obs : Observation) (m : Manifest) (p : Prefs) (project : Project)
    (s : Session) : ObservedLaunch :=
  { launch := inject m p project s, observation := obs }

/-- The GUI Lens and segment metadata inject zero prompt-visible bytes. The guarantee
    holds by construction: `inject` takes no observation. -/
theorem observation_independent (o₁ o₂ : Observation) (m p project) (s₁ s₂ : Session) :
    (launchWith o₁ m p project s₁).launch.visible =
      (launchWith o₂ m p project s₂).launch.visible := rfl

/-! ## Parked-segment handoff

A Claude request is its system blocks followed by the conversation history. Handing a
thread to another Agent writes a capsule into that Agent's conversation only. -/

abbrev Msg := String

structure Conversation where
  model : String
  system : List Msg
  history : List Msg

/-- Prompt bytes of the next request. The model selects the cache but is not a message. -/
def messages (c : Conversation) : List Msg := c.system ++ c.history

structure Handoff where
  claude : Conversation
  other : Conversation

inductive HandoffEvent where
  /-- Build a capsule from Claude's history with any summarizer and give it to the
      other Agent. -/
  | park (capsule : List Msg → List Msg)
  /-- Any amount of work on the other Agent, including rewriting its history. -/
  | otherWork (work : Conversation → Conversation)
  /-- Return to Claude with new user message(s). -/
  | resume (msgs : List Msg)
  /-- A Claude turn: append messages to its history. -/
  | claudeTurn (msgs : List Msg)
  /-- Tier 1 model switch: same history, different model. -/
  | switchModel (model : String)

def handoffStep (h : Handoff) : HandoffEvent → Handoff
  | .park capsule => { h with other := { h.other with history := h.other.history ++ capsule h.claude.history } }
  | .otherWork work => { h with other := work h.other }
  | .resume msgs | .claudeTurn msgs => { h with claude := { h.claude with history := h.claude.history ++ msgs } }
  | .switchModel model => { h with claude := { h.claude with model := model } }

/-- Events that run while Claude is parked. -/
def Parked : HandoffEvent → Prop
  | .park _ | .otherWork _ => True
  | _ => False

theorem parked_untouched (h : Handoff) (es : List HandoffEvent) (hp : ∀ e ∈ es, Parked e) :
    (es.foldl handoffStep h).claude = h.claude := by
  induction es generalizing h with
  | nil => rfl
  | cons e es ih =>
    rw [List.foldl_cons, ih _ (fun e' he => hp e' (List.mem_cons_of_mem _ he))]
    have := hp e List.mem_cons_self
    cases e <;> simp_all [Parked, handoffStep]

/-- Every event keeps Claude's previous messages a prefix of its next ones. -/
theorem claude_append_only (h : Handoff) (es : List HandoffEvent) :
    messages h.claude <+: messages (es.foldl handoffStep h).claude := by
  induction es generalizing h with
  | nil => exact List.prefix_refl _
  | cons e es ih =>
    refine List.IsPrefix.trans ?_ (ih (handoffStep h e))
    cases e <;> simp [handoffStep, messages, List.prefix_append]

/-- Claude's last request before parking is a prefix of its first request after
    returning, for any capsule and any work on the other Agent. -/
theorem parked_prefix_preserved (h : Handoff) (capsule : List Msg → List Msg)
    (work : List HandoffEvent) (hw : ∀ e ∈ work, Parked e) (msgs : List Msg) :
    let after := ((HandoffEvent.park capsule :: work) ++ [HandoffEvent.resume msgs]).foldl
      handoffStep h
    messages h.claude <+: messages after.claude ∧
      messages after.claude = messages h.claude ++ msgs := by
  have hc := parked_untouched (handoffStep h (.park capsule)) work hw
  have h0 : (handoffStep h (.park capsule)).claude = h.claude := rfl
  rw [h0] at hc
  simp only [List.foldl_append, List.foldl_cons, List.foldl_nil]
  generalize work.foldl handoffStep (handoffStep h (.park capsule)) = mid at hc
  simp only [handoffStep, messages, hc]
  constructor
  · rw [← List.append_assoc]; exact List.prefix_append _ _
  · simp

/-- A Tier 1 model switch with append-only history keeps the prior request's messages a
    prefix. The cache is per model, so this claims the message prefix, not warmth. -/
theorem model_switch_prefix (c : Conversation) (model : String) (msgs : List Msg) :
    messages c <+: messages { c with model := model, history := c.history ++ msgs } := by
  simp [messages, List.prefix_append]

end Diri.Claude
