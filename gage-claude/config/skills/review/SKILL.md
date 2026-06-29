---
description: Review Gage issues and close them as completed or skipped.
disable-model-invocation: true
---

## Required tools

This task uses the following tools:

- `mcp__plugin_gage_gage__Query` - SQL interface to Gage data. Use it to list
  issues and to fetch one issue's full detail:
  - `SELECT id, name, title, status FROM issue WHERE status = 'open'` lists open
    issues
  - `SELECT * FROM issue_report(<id>)` returns one row with the full picture of
    an issue: description (with `scanner:/...` refs already resolved), related
    notes (as a JSON array), and history events (as a JSON array)
- `mcp__plugin_gage_gage__IssueClose` - mark an issue closed with a reason
  (completed or skipped) and a comment

## Instructions

Gage scans Claude Code sessions and records anything that warrants user
attention as an issue. An issue is open or closed.

Examine the open issues and present a short summar for the user with
recommendations for proceeding.

1. Query the `issue` table for open issues
2. Use the issue title to help triage the list. Call `issue_report(<id>)` to get
   the full report for issues that look most important
3. Work with user as needed to resolve the issue, either by completing it (e.g.
   applying recommended fix or another solution) or by skipping it. Call
   `mcp__plugin_gage_gage__IssueClose` with reason completed or skipped along
   with a comment explaining either how the issue was completed or why it was
   skipped

## Guidelines

The list of issues is advisory. Provide honest and accurate feedback to help the
user address issues based on user values and priorities.

The issue description and fix MAY contain errors. Reported evidence MAY be
outdated. Verify evidence and conduct further analysis to arrive at a correct
fix. If the issue does not warrant any action, close it with a "skipped" reason
with a comment.

Do not close an issue as "completed" until you have confirmed with the user that
the underlying issue is resolved. If the user cannot confirm that the issue is
resolved, consider waiting for more scanner evidence and re-evaluate the issue
later.
