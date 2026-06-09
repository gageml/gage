---
description: |
  Review Gage issues and close them as completed or skipped.
---

## Required tools

This task uses the following tools:

- `mcp__plugin_gage_gage__ListIssues` - list issues, open by default
- `mcp__plugin_gage_gage__GetIssue` - fetch one issue's full detail:
  description, originating scanner, and linked evidence and comments
- `mcp__plugin_gage_gage__CloseIssue` - mark an issue closed with a reason
  (completed or skipped) and a comment

## Instructions

Gage scans Claude Code sessions and records anything that warrants user
attention as an issue. An issue is open or closed.

Walk the user through their open issues and close each one according to the
user's decision.

The Gage tools (ListIssues, GetIssue, CloseIssue) are provided by the gage MCP
server and load on demand. If they are not already available, load them first
with a ToolSearch keyword query (e.g. `gage issues`). Do not call them by a
guessed fully-qualified name.

1. Call ListIssues to see whats open

2. Work with user to identify highest value issues and start there

3. Call GetIssue to show issue detailed description, evidence, and comments

4. Work with user as needed to resolve the issue, either by completing it (e.g.
   applying recommended fix or another solution) or by skipping it. Call
   CloseIssue with reason completed or skipped along with a comment explaining
   either how the issue was completed or why it was skipped

The list of issues is advisory. Provide honest and accurate feedback to help
the user address issues based on user values and priorities.

The issue description and fix MAY contain errors. Reported evidence MAY be
outdated. Verify evidence and conduct further analysis to arrive at a correct
fix. If the issue does not warrant any action, close it with a "skipped" reason
with a comment.

Do not close an issue as "completed" until you have confirmed with the user
that the underlying issue is resolved. If the user cannot confirm that the
issue is resolved, consider waiting for more scanner evidence and re-evaluate
the issue later.
