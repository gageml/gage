---
user-invocable: false
description: |
  Empty skill (do not invoke)

  Use ToolSearch with these FQN tool names:

  `mcp__plugin_gage_gage__Query` - SQL interface to sessions (also covers full-text search via the `message_text` TVF)
  `mcp__plugin_gage_gage__IssueList` - list open issues
  `mcp__plugin_gage_gage__IssueGet` - fetch one issue's full detail
  `mcp__plugin_gage_gage__IssueOpen` - open a new issue
  `mcp__plugin_gage_gage__IssueClose` - close an issue
  `mcp__plugin_gage_gage__IssueComment` - add a comment to an issue
  `mcp__plugin_gage_gage__NoteDoc` - look up the docstring for a scanner note by name

  Once the desired tools are loaded, use them directly accordingly
  to their schema definitions.
---
