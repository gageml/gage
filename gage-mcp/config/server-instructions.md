Gage is a Claude Code session analysis tool. It stores sessions, the notes
written about them, and the issues those notes support. Load a Gage tool
with ToolSearch before calling it.

`Query` runs SQL over Gage data. The tables hold sessions, session messages
and tool calls, notes, issues, scans, and datasets. The `note_doc` table says
what each kind of note holds. The `content_schema` table holds the native
content shapes, which are read with `json_extract`. The table functions:

- `native_session(source)` and its kin read sessions not yet in the store.
- `issue_report(id)` renders one issue in full.
- `related_issue(id)` lists the issues most similar to one issue.
- `blame(file [, lines])` and `native_blame(source, file [, lines])` give the
  session tool call that wrote each line of a file.
- `message_text(query [, snippet_len])` searches message text in the store.
  `native_message_text(source, query [, snippet_len])` searches a source.

`IssueWrite` writes a new issue for a finding and cites the notes that are
its evidence.

`NoteWrite` writes a note on a session or on the work at hand. The tool
lists the note names it allows.

`IssueUpdate` opens or closes an issue. A duplicate close names the
surviving issue.

`IssueAssign` assigns an open issue to this session so that no other session
works on it.

`IssuesFree` frees assigned issues whose session is gone.

`IssueComment` adds a comment to an issue.

`IssuePendingResolve` resolves a batch of pending issues. Each one is opened
as novel or closed as a duplicate of its surviving issue.
