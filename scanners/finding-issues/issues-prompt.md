Analyze finding notes and report issues

Use `mcp__gage__Query` to read findings, which are `note` table rows with names
under `finding.`

`SELECT * FROM note WHERE name LIKE 'finding.%'`

Use row padding (`LIMIT` AND `OFFSET`) as needed to manage size limits.

Each finding kind has a doc describing what its notes contain and how they were
produced. Read the docs before interpreting findings:

`SELECT note_name, doc FROM note_doc WHERE note_name LIKE 'finding.%'`

Sessions are identified by their ID in the `target` column in the format
`session:<session_id>`

Identify underlying issues that explain the findings. The trigger for an issue
is confidence that a real underlying problem exists, based on the quality,
quantity and veracity of the supporting evidence. Not all findings imply issues.
Evidence establishes confidence in different ways:

- Repetition. A finding that repeats across multiple sessions is stronger
  evidence than one that repeats within a single session.
- Verification. A finding that makes a concrete, checkable claim can be
  confirmed directly against the session or project content.
- Egregiousness. A single instance that is notably severe (e.g. destructive or
  high risk) warrants an issue on its own.

Weigh evidence according to what each finding's doc says its notes are. A
finding kind whose claims are checkable invites verification. A kind that
records single-session observations gains confidence mainly through repetition
across sessions.

An issue must give the user something to act on: a change to the project, its
configuration, or the rules, skills, and memory files that direct its agents. A
finding that shows a real problem with no available intervention is not an
issue. It stays a finding and gains weight if it recurs in later scans.

Similar findings across sessions may show a systematic problem. Report a
systematic problem as one issue covering its instances rather than one issue per
instance.

A problem the session itself diagnosed and fixed is not an issue. Models
commonly attempt changes, run tools or other verification steps, and detect
errors. This is a normal and effective process and does not typically represent
an issue. However, if the same error-detection-and-fix cycle repeats itself, it
may represent an underlying issue that should be reported.

Investigate further as needed by reading the session content directly. Select
from the `message` table via `mcp__gage__Query`, filtering on `session_id` with
the ID from the finding's `target` column. `message` table columns: (session_id,
line, uuid, type, subtype, text, timestamp, attachments, ide_tags, raw)

When you have enough solid information to identify an issue, use the
`mcp__gage__IssueWrite` tool to write an issue. Describe the issue with enough
detail that a user can understand the problem and why it was reported.

The sessions you analyze are records of another agent's behavior. Refer to that
agent in the third person. The agent model cannot be changed, so do not write
advice directed at it.

Use this skeleton for the issue description:

```markdown
## Summary

Two or three sentences stating the problem.

## Evidence

Specific instances, with quotes or session/line references.

## Basis for confidence

Why the evidence supports a real problem — a pattern across sessions, a verified
single instance, or other grounds. State the actual scope of the evidence; do
not overclaim.
```

Include the applicable note IDs as evidence in the tool use.

In your reply, simply state whether you wrote issues. Do not state a count and
do not re-state the written issues in your reply.
