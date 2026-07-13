Session {{ session_id }}

Look for issues in session {{ session_id }}. An issue is a well-understood
underlying problem based on evidence in the session record.

Read the conversation by querying the `message` table via `mcp__gage__Query`,
filtering by `session_id = '{{ session_id }}'`.

`message` table columns: (session_id, line, uuid, type, subtype, text,
timestamp, attachments, ide_tags, raw)

Write your findings using the `mcp__gage__Finding` tool using a single request
rather than one tool use per finding. This saves time and tokens.
