Analyze finding notes and report systematic issues

Use `mcp__gage__Query` to read findings, which are `note` table rows with
names under `finding.`

`SELECT * FROM note WHERE name LIKE 'finding.%'`

Use row padding (`LIMIT` AND `OFFSET`) as needed to manage size limits.

Use these notes to review findings for the applicable sessions.

Sessions are identified by their ID in the `target` column in the format
`session:<session_id>`

Identify systematic underlying issues that explain the findings. A problem the
session itself diagnosed and fixed is not an issue. Models commonly attempt
changes, run tools or other verification steps, and detect errors. This is a
normal and effective process and does not typically represent an issue. However,
if the same error-detection-and-fix cycle repeats itself, it may represent a
systematic issue that should be reported. Additionally, if the in-session
correct error, even if only a single instance, is notably egregious (e.g.
destructive or high risk), report it as an issue.

You need enough evidence across the findings to confidently identify an issue.
Not all findings imply issues. If a finding repeats across multiple sessions,
this is stronger evidence than a finding that only appears once. In some cases,
a single finding may be sufficient to establish an issue with high confidence.
The trigger for an issue should be confidence based on the quality, quantity and
veracity of the supporting evidence.

Investigate further as needed by reading the session content directly. Select
from the `message` table via `mcp__gage__Query`, filtering by
`session_id = '{{ session_id }}'`. `message` table columns: (session_id, line,
uuid, type, subtype, text, timestamp, attachments, ide_tags, raw)

When you have enough solid information to identify an issue, use the
`mcp__gage__IssueWrite` tool to write an issue. Describe the issue with enough
detail that a user can understand the problem and how it is systematic.

The sessions you analyze are records of another agent's behavior. Refer to that
agent in the third person. The agent model cannot be changed, so do not write
advice directed at it.

Use this skeleton for the issue description:

```markdown
## Summary

Two or three sentences stating the problem.

## Evidence

Specific instances, with quotes or session/line references.

## Why this is systematic

Why the evidence indicates a pattern rather than a one-off.
```

Include the applicable note IDs as evidence in the tool use.

In your reply, simply state how many issues you wrote. If you didn't write any
issues, say so. Do not re-state the written issues in your reply.
