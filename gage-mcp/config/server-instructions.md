Gage is a coding-agent session analysis tool. It stores sessions, the notes
written about them, and the issues those notes support. Where the host lists a
Gage tool without its schema, load the tool before calling it.

`query` runs SQL over Gage data. The tables hold sessions, session messages
and tool calls, notes, issues, scans, and datasets. The `note_doc` table says
what each kind of note holds. The `content_schema` table holds the native
content shapes, which are read with `json_extract`. The table functions:

- `issue_report(id)` renders one issue in full.
- `related_issue(id)` lists the issues most similar to one issue.
- `blame(file [, lines])` gives the session tool call that wrote each line of a
  file.
- `message_text(query [, snippet_len])` searches message text.

`query` reads the main store. Its `source` parameter names another source to
read in its place: another Gage store, or a native source, which holds sessions
not yet in the store. A named source is read only. Every write tool writes the
main store alone.

`issue_write` writes a new issue for a finding and cites the notes that are
its evidence.

`note_write` writes a note on a session or on the work at hand. The tool
lists the note names it allows.

`issue_update` opens or closes an issue. A duplicate close names the
surviving issue.

`issue_assign` assigns an open issue to this session so that no other session
works on it.

`issues_free` frees assigned issues whose session is gone.

`issue_comment` adds a comment to an issue.

`issue_pending_resolve` resolves a batch of pending issues. Each one is opened
as novel or closed as a duplicate of its surviving issue.
