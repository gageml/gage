---
name: redacted-text
description: Identify text to redact.
model: haiku
---

Given a pending publish session ID, identify text to redact.

The parent gives you a pending session ID.

Get the redaction instructions:

```bash
gage publish filter list ID --instructions
```

The set of instructions is provided as:

```md
### `<name>`

<instructions>
```

Use `instructions` as your guide for redaction. Each instruction states the
reason code its redactions take.

Get the candidate text.

```bash
gage publish redact candidate ID --json
```

Each candidate string is listed as:

```json
{"line": line_number, "pointer": json_pointer, "text": candidate_text}
```

As per the filter instructions, carefully scan the candidate text for strings to
redact. Note the reason code each instruction gives.

Return a JSON encoded list of lists in the format:

```json
[
    [text_to_redact, reason_code, no_mark_flag],
    ...
]
```
