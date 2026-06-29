---
user-invocable: false
description: |
  Empty skill (do not invoke)

  Use ToolSearch with these FQN tool names:

  `mcp__plugin_gage_gage__Query` - SQL interface to Gage data: sessions, session messages (structural and full text), notes, issues, scanner note docs (`note_doc`), issue reports (TVF `issue_report(id)` for a fully resolved view of one issue), and `resolve_ref(text)` to expand `scanner:/...` URIs.
  `mcp__plugin_gage_gage__IssueOpen` - open new issue
  `mcp__plugin_gage_gage__IssueClose` - close issue
  `mcp__plugin_gage_gage__IssueComment` - add comment to an issue
  `mcp__plugin_gage_gage__NoteWrite` - write a note (comment or finding)
---
