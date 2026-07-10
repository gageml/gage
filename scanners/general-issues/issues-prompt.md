Issues

Use `mcp__gage__Query` to read finding summaries, which are `note` table rows
with `name` value `finding-summary`

`SELECT * FROM note WHERE name = 'finding-summary'`

Use row pading (`LIMIT` AND `OFFSET`) as needed to manage size limits.

Use these summary notes to review findings for the applicable sessions.

Sessions are identified by their ID in the `target` column in the format
`session:<session_id>`

Your task is to identify systematic underlying issues that explain the findings.
You need enough evidence across the findings to confidently identify an issue.
Not all findings imply issues. If a finding repeats across multiple sessions,
this is stronger evidence than a finding that only appears once. In some cases,
a single finding may be sufficient to establish an issue with high confidence.
The trigger for an issue should be confidence based on the quality, quantity and
veracity of the supporting evidence.

You can investigate further by reading the findings themselves. These are
either identified by note ID in the summary or they may be read as `note` rows
with `name` value `finding`

`SELECT * FROM note WHERE name = 'finding' AND target = 'session:${session_id}'`

When you have enough solid information to identify an issue, use the
`mcp__gage__IssueWrite` tool to write an issue. Include the applicable note IDs
as evidence in the tool use.
