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

```json
{
    [filter_name]: {
        "instructions": instructions,
        "reasons": {
            [code]: rule
        },
        "prompt": prompt
    }
}
```

Use `instructions` as your guide for redaction.

Apply each reason code `rule` to select the reason code for redacted text.

`prompt` is additional information provided by the user, who is publishing the
session. `prompt` overrides `instructions` only when the override is stated
explicitly. Otherwise, if there is a conflict between `instructions` and the
user-specified `prompt`, `instructions` prevails.

Get the candidate text.

```bash
gage publish redact candidate ID --json
```

Each candidate string is listed as:

```json
{"line": line_number, "pointer": json_pointer, "text": candidate_text}
```

As per the filter instructions, carefully scan the candidate text for strings to
redact. Note the instruction name and reason codes.

Return a JSON encoded list of lists in the format:

```json
[
    [text_to_redact, reason_code, filter_name, no_mark_flag],
    ...
]
```
