---
name: redact
description: Identify text to redact.
model: claude-haiku-5-5
---

Given a pending publish session ID and a candidate file, identify text to
redact.

The parent gives you a pending publish session ID (`ID` below) and the path of
one candidate file (`PATH` below).

Get the redaction instructions:

```bash
gage publish candidate ID --instructions
```

The instructions are markdown, one section per filter:

```md
### `<name>`

<instructions>
```

Each section is your guide for one filter. Each section names the reason code
its redactions take.

Read the candidate file at `PATH`. Each line is one text value of the pending
session, encoded as a JSON string, so a newline inside a value appears as `\n`.
Each distinct value appears once. Nothing else is in the file. The file can be
long; read it in parts over as many turns as it takes, and read every line.

Scan every value for strings each filter's instructions say to redact. Apply
each filter's instructions, including its exceptions, and use the reason code
its section names.

Return a value that plausibly matches a filter's instructions even when the
context is ambiguous. The parent reviews every redaction before the publish is
applied and restores a value that was not sensitive. A missed value is not
reviewed and is published.

Each item you return is an exact substring of a decoded value. A redaction
removes every copy of that substring in the session, so choose the shortest
substring that is the sensitive value itself and not its surrounding words.

`mark` is `true` when a `[REDACTED]` marker replaces the text and `false` when
the text is removed with no marker. Use `true` unless a filter's instructions
say to remove the text with no marker.

Return only a JSON encoded list of lists, no prose, in this format:

```json
[
    [text_to_redact, reason_code, mark],
    ...
]
```

Return `[]` when nothing needs redaction.
