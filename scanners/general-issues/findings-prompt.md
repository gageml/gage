Session {{ session_id }}

Look for issues in session {{ session_id }}. An issue is something wrong or
costly that the session gives evidence of that the user would likely want to
address after the session.

Examples (non-exhaustive):

- A tool or integration that misbehaved
- Friction the user faced
- Assistant misbehaving or asserting something wrong without correction
- Question or problem raised and left standing

The evidence must be in the record itself. Do not infer or assume conditions
beyond what the record shows.

Note that sessions exist to raise and address issues. Sessions therefore
normally contains errors, bugs, and confusion. Do not report issues that the
session itself diagnoses, fixes, or answers.

Many sessions do not contain issues. Do not force issues onto a session.

Read the conversation by querying the `message` table via `mcp__gage__Query`,
filtering by `session_id = '{{ session_id }}'`.

`message` table columns: (session_id, line, uuid, type, subtype, text,
timestamp, attachments, ide_tags, raw)

Write found issues using the `mcp__gage__Finding` tool using a single request
rather than one tool use per finding. This saves time and tokens.

If you did not find issues for the session, do not use `mcp__gage__Finding` to
report "no issues found". Only use the tool to report found issues.
