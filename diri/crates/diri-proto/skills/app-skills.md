# App Skills

In Claude Code and Codex, pass the action names below to Diri’s `act` tool with an `arguments` object. Use `get_skill` with `action` for its schema, or with no arguments to list actions and guides. In a Diri terminal, `"$DIRIJOR_CLI" mcp-tools` prints the CLI catalog.

An App Skill is a task in a Mac app that computer use learned once and saved, per app, as a parameterized list of `run_steps` steps. Diri replays it later with no model in the loop: no screenshots, no reasoning, a few hundred milliseconds per step instead of a read-think-act loop. You are the model: you teach, edit and repair skills; the Engine stores, verifies and replays them through the same background path as the `computer` tool (your cursor badge, your app leases, never the person's focus).

All of it is one tool, `app_skills`, with an `action`.

## Check before you click

Before doing a task in an app by hand, and always before doing one more than once, call `app_skills {"action": "list"}` (or `{"action": "list", "app": "Numbers"}`). If a skill fits, run it:

```json
{"action": "run", "app": "com.example.Invoices", "skill": "create_invoice", "args": {"customer": "Ada Lovelace", "amount": 120}}
```

Many items: pass `args_list` (one args object per item, at most 50). Each item runs in turn and reports its own outcome; by default the run stops at the first failed item (`continue_on_error: true` keeps going). A skill marked `destructive` (it deletes, sends, pays, submits) over more than one item is refused until you pass `confirm: true`, and you may only pass that after the person agreed to that exact batch.

## Teach one: learn, trace, save

1. `{"action": "learn", "app": "Invoices", "goal": "Create an invoice for a customer and amount"}` puts your session in learn mode for that app.
2. Do the task once with the `computer` tool, exactly as you normally would (`get_window_state`, then act with element tokens or `run_steps`). Only successful actions are recorded; reads are not. Use real example values for whatever varies.
3. `{"action": "trace"}` returns the recorded actions as a draft: element tokens are already turned into selectors (`role`, `name`, `nth`, `window`), every typed or set literal is listed in `candidate_params`, and `warnings` names steps that still need you (an unlabeled element, a pixel click).
4. Edit the draft into a skill and save it:

```json
{"action": "save",
 "skill": {
   "name": "create_invoice",
   "goal": "Create an invoice for a customer and amount",
   "params": [{"name": "customer", "type": "string", "description": "Customer name as shown in the list"},
              {"name": "amount", "type": "number", "description": "Total in the account currency"}],
   "steps": [
     {"tool": "click", "args": {"role": "button", "name": "New Invoice"}, "intent": "Open a blank invoice",
      "expect": [{"role": "textfield", "name": "Customer"}]},
     {"tool": "set_value", "args": {"role": "textfield", "name": "Customer", "value": "{customer}"}, "intent": "Fill in the customer"},
     {"tool": "set_value", "args": {"role": "textfield", "name": "Amount", "value": "{amount}"}, "intent": "Fill in the amount"},
     {"tool": "click", "args": {"role": "button", "name": "Save"}, "intent": "Save the invoice",
      "expect": [{"text": "Saved"}]}
   ],
   "destructive": false},
 "example_args": {"customer": "Ada Lovelace", "amount": 120}}
```

`save` validates (every param used, every `{placeholder}` declared, only batchable tools, selectors not tokens or pids, at most 64 steps), then **verifies** by replaying the skill with `example_args`. It is saved as `verified` only if that replay passes; otherwise you get the failing step and the driver's error, and nothing is saved. A destructive skill is verified `until_last` by default: every step but the final one, leaving the app mid-flow, which you then cancel or finish (or tell the person). `verify: "none"` saves it `unverified`. Saving under an existing name keeps the old version for `rollback` and bumps the version.

Writing good steps:

- Target by `role` + `name` (add `nth` when several match, `window` for a dialog or second window). Never element tokens, `pid` or `window_id`: they only last one launch.
- Give every step an `intent` sentence. It is never sent to the app; it tells whoever repairs the skill what the step was for.
- Add `expect` checks after steps whose effect matters (a dialog opened, a row appeared), and `wait_for` before steps that need something to load. Skill-wide `preconditions` and `postconditions` are check objects too.
- `{param}` placeholders go in any string value; `{{` and `}}` are literal braces. A value that is exactly `"{amount}"` with a number param is sent as a number.
- Pixel clicks (`x`, `y`) only when no selector reaches the control; mark the step `"pixel": true`. Such a skill never counts as verified.
- Never save passwords, tokens, card numbers or other secrets as literals. Make them params, and don't pass secrets in `args` unless the person gave them to you for this.
- Keep it to one task. Compose bigger jobs from several skills, or loop with `args_list`.

`{"action": "abandon"}` leaves learn mode without saving.

## When a run breaks: repair

A failed run answers with `ok: false`, `failed_step`, that step's `intent`, `driver_error`, the skill's `goal`, a compact read of the app's current `window`, and `saved_skill` (the whole skill as saved). Apps change: a button gets renamed, a dialog appears, a field moves into a sheet.

1. Read the payload; if you need more, look at the window with the `computer` tool.
2. Fix the step(s) in `saved_skill` (selector, an added `wait_for`, a dismissed dialog), and call `save` with it and `example_args`. Verification replays it; the version goes up.
3. If the run failed because of this account or this data (no permission, a record that doesn't exist), not because the app changed, don't overwrite the skill: report it.

After three failed runs in a row a skill is marked `broken`. When the app's version changes, its skills turn `unverified` until their next clean run. `history` shows versions and recent runs (never argument values); `rollback {app, skill, version}` restores an earlier version; `forget {app, skill}` deletes one.

## Rules

- Skills run in the background like every `computer` call: no foreground, no desktop scope, never the person's cursor or keyboard. An app another Agent is driving returns `computer_app_busy`.
- Act only in apps the person pointed you to or you launched. A skill is not permission to run it on someone else's data.
- The person sees every skill under Settings › Agents › Computer use › Learned apps, and can delete them there.
