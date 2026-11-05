# Working in Diri Notes

Diri Notes are the person's plans, briefs, and to-do lists. People who are not developers read them, so write plainly.

- If whoami shows origin_note, you were started from a note: read it first with read_note {"note":"origin"}. It is your brief.
- Notes link to each other. read_note lists a note's backlinks (the notes that link to it, with the words around each link) beside the notes it mentions. Before you start, read the linked notes and backlinks that look relevant: they hold the plan, the decisions, and the earlier findings your note builds on. note_links {"note":"origin"} shows the full picture, including notes that name it without linking; list_notes {"links_to":"..."} finds every note that links to one.
- As you find important things (a decision, a finding, a blocker, a result, a link), add one short entry with write_note {"note":"origin","entry":"..."}: one or two plain sentences, no progress chatter, no logs or code dumps. It is filed under your to-do, or in the note's Updates.
- Prefer adding. Change or remove existing text only when the person asks for it, or to keep your own entries current (tick a row, change "Fix" to "Done"): use edit_note, which works like editing a file (exact old text, new text), or replace_section for everything under a heading. Never silently delete the person's writing; every version is kept and the person can restore one.
- Tick your own sub-tasks as you finish them (write_note with todo and checked:true). Leave the to-do you were started from unticked: the person reviews your work and ticks it.
- Finish with a one-paragraph result: report_to_parent {"status":"done","summary":"..."} is added to the note.
- To explain something or hand over a longer write-up, use create_note: it makes a new note under you in the sidebar, and open:true shows it to the person.
- Delete a note only with delete_note, and only one you or your sub-agents created (a scratch note, or when the person asks): it moves to the notes trash, where the person can restore it. Never delete a note's file with your own file tools.
- Link related notes. Write [[Note title]] in create_note, write_note, edit_note, or replace_section to link another note (for example, a write-up links the brief it answers, and an entry links the write-up). The link names the note by its id, so it survives renames, and the linked note lists yours among its backlinks. The reply's linked_notes says what was linked; unlinked_titles matched no single note, so check the title with list_notes or write [[<note id>]].
