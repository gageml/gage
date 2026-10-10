---
user-invocable: false
description: |
  Empty skill (do not invoke)

  Use ToolSearch with these FQN tool names:

  `mcp__plugin_gage_gage__Query` - SQL interface to Gage data: sessions, session messages, notes, issues, scanner note docs (`note_doc`), issue reports (TVF `issue_report(id)`, the full view of one issue), and candidate duplicates (TVF `related_issue(id)`, the issues most similar to one issue).
  `mcp__plugin_gage_gage__IssueWrite` - write a new open issue, citing evidence notes
  `mcp__plugin_gage_gage__NoteWrite` - write a note, on a session or on the work at hand; the tool lists the names it allows
  `mcp__plugin_gage_gage__IssueUpdate` - open or close an issue; a duplicate close names its surviving issue
  `mcp__plugin_gage_gage__IssueComment` - add a comment to an issue
---
