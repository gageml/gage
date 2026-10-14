---
description: |
  Consult the session record behind a piece of code. Use whenever the user
  asks why code is the way it is, what the reasoning or intent behind a
  change was, when and why something changed, whether an alternative was
  considered or rejected, or what was discussed before a decision; and
  before explaining a non-obvious design choice from the code alone. The
  record holds the discussion, the model's thinking, and the edits that
  produced each line, and is consulted with the same instinct as git log
  and code comments.
disable-model-invocation: false
argument-hint: "FILE[:LINES] [question]"
---

The Claude Code sessions that wrote a project's code are a record of how the
code came to be: the discussion before each change, the model's thinking, and
the edits themselves. `gage blame` maps lines of a file to the session and the
span of that session that produced them. Consult it with the same instinct as
`git log` and code comments. The record is the primary source for rationale;
git history is the source for when a change landed; comments are the source
for invariants the code enforces. The lookup takes under a second, so run it
rather than deliberate over it.

## Procedure

1. Locate the lines. The user's reference, or a search of the code, gives a
   file and a line selection: a line (`42`), a range (`10-20`), or a list
   (`10-20,42`).

2. Run the command and read the JSON:

   ```
   gage blame -l <LINES> <FILE> --json
   ```

   Each row is one source line with `session_id`, `session_prefix`, `span`
   as `[start, end]`, `timestamp`, and `match`. Group the rows by session and
   span. A `match` of `content` means the call's input carries the line's
   text. A `match` of `file` means the latest write to the file in the line's
   window stood in, which is weaker (a shell command's recorded diff lists
   every file changed since the last snapshot, not only what that command
   wrote, so a file match can name a session that touched the file nearby but
   did not write the line). A null session means no recorded session
   wrote the line; git history and comments are then the only record.

3. Read each span through the `mcp__plugin_gage_gage__Query` tool. The span
   runs from the human prompt before the call to the call itself:

   ```sql
   SELECT line, type, subtype, substr(text, 1, 1200) AS text
   FROM native_message('claude:')
   WHERE session_id = '<session_id>' AND line BETWEEN <start> AND <end>
   ORDER BY line;
   ```

   Rows with subtype `thinking` are the model's reasoning. Rows with subtype
   `tool_use` are the edits; the full edit is `input_raw` in
   `native_tool_use('claude:')` at the same `session_id` and `line`. Read full
   `text` for the rows that matter.

4. Widen when the span's prompt is an approval such as "go ahead" or "do
   that". The decision was made earlier. Read back through the preceding
   human prompts and the assistant turns between them until the discussion
   that settled the choice appears:

   ```sql
   SELECT line, substr(text, 1, 400) AS text
   FROM native_message('claude:')
   WHERE session_id = '<session_id>' AND type = 'user' AND subtype = 'text'
     AND line < <start>
   ORDER BY line DESC LIMIT 8;
   ```

   Then read the assistant turns between the prompt that raised the question
   and the span.

5. If a session is not in the native source, which happens once Claude Code
   has pruned it, repeat step 2 with `--stored` and read with the `message`
   and `tool_use` tables in place of the native functions.

6. Answer from the record. State the rationale as the record gives it, and
   quote where the wording matters. Cite each source as
   `session <session_prefix> L<start>-<end>`. Distinguish what the record
   states from what is inferred from it. Where the record is silent, say so
   and fall back to git history and comments rather than guessing.

## Invoked directly

`/gage:blame FILE[:LINES] [question]` runs the procedure on the named file
and lines. With no lines, the whole file is blamed; with a question, the
answer addresses it.
