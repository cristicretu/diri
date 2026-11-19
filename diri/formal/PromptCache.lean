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

end Diri.Claude
