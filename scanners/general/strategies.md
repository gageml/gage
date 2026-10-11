# Session record strategies

Potential changes to how the `general` scanner presents a session to its finding
agent. None of this is implemented in the scanner. The scanner sends a short
prompt naming the session, and the agent reads the session through its Query
view.

## Motivation

Letting the agent pull the session line by line through tool calls is costly.
Each tool call re-submits the growing context, and context limits trigger
compaction and re-reads. The alternative is to put a compressed rendering of the
session, a _record_, into the initial prompt so the agent can read most of the
session at once and query only the lines it needs in full.

How much to send is unknown. Too little lowers recall. Too much lowers both
precision and recall as the model's performance degrades with context size.
Which point on that slider works best is a question for formal evals with golden
datasets. The design below gives the scanner levers for that experiment without
committing to an answer.

## Shape

Two scanner params:

- `strategy` names how much of the session goes into the prompt.
- `budget` caps the record in tokens. `0` sizes the record to the agent's free
  context.

One prompt template with one conditional section. When a record is present, the
prompt explains the record's format and instructs the agent to query specific
lines rather than page through the session. When it is absent, the prompt
instructs the agent to read the session through Query.

One work key regardless of strategy, or one key per strategy so runs under
different strategies do not mask each other's work. This is undecided. The
watermarks design says a key changes when a mode reads the input differently,
and a strategy is such a mode.

## Strategies

Each strategy is a rule set for `Session::compress(budget, rules)`, or no rules
at all. A message kind no rule matches is excluded from the record.

`none`. No record. The agent reads the session through Query. This is what the
scanner does today.

`index`. Header lines only: every message's line number and kind, no content.

```rune
#{
    "user": #{ header: true },
    "assistant": #{ header: true },
    "system": #{ header: true },
}
```

`abridged`. Content kept per weighted rules and cut to fit the budget. This is
what the legacy scanner's roadmap mode sends. Human intent (`user.text`) and the
assistant's reasoning carry the primary signal for session quality; tool
activity is context. `min` is a floor that survives budget pressure, since
demotion drops lowest-weight rules first. No `max`: the budget is the ceiling.

```rune
#{
    "user.text": #{ min: 300, weight: 5 },
    "assistant.thinking": #{ min: 300, weight: 4, priority: ["start", "end"] },
    "assistant.text": #{ min: 300, weight: 4 },
    "user.tool_result": #{ min: 200, weight: 2 },
    "assistant.tool_use": #{ min: 200, weight: 2 },
}
```

`full`. Every message with no per-message cap. The budget alone bounds the
record, so `full` cannot exceed the context window.

```rune
#{ "user": #{}, "assistant": #{}, "system": #{} }
```

## Budget

The record's token budget is the `budget` param when set. Otherwise it is 80% of
the agent's free context as reported by `model_context(model)`, capped at
800,000 tokens. The compressor converts tokens to chars at a fixed 2.2 chars per
token.

## Prompt additions

When a record is present the prompt carries this legend:

> An abbreviated record of the session is included at the end of this prompt.
> Every message appears under a `[L<line> <type> <subtype>]` header, where
> `<line>` is the message's session line number; use these numbers when citing
> lines in findings. Message content may be shortened or omitted:
>
> - a header with no body means the content was omitted
> - `… [+N chars]` marks content cut at a cap, with the omitted char count
> - `… [N chars omitted] …` marks the elided middle of a thinking block
> - tool call messages (subtype `tool_use`) show the tool name and its input
>   fields, truncated
> - tool result messages (subtype `tool_result`) show only the result's first
>   line

and this instruction in place of the read-via-Query instruction:

> The record is often evidence enough; when a shortened message looks relevant
> to a possible finding, read its full text before writing the finding by
> querying the `message` table via `mcp__gage__Query`, filtering by
> `session_id = '<id>'` and the line numbers of interest. Keep queries targeted
> at specific lines or narrow ranges. Do not page through the whole session —
> the record already covers all of it.

The record itself goes at the end of the prompt under a `## Session record`
heading.

## Runtime dependencies

- `Session::compress(budget, rules)` on the rethink runtime, over the scan's
  query context, with `.lines(start, end)` to constrain the record to the
  session's unseen lines. The legacy compressor takes the range from the session
  value; rethink sessions carry no range.
- `model_context(model)` for the default budget. The legacy implementation runs
  `claude -p /context`, which ties it to the Claude driver.
- Scanner params, which the rethink runtime has.

## Open questions

- Whether the note records the strategy that wrote it. Tool callbacks run
  without task params in legacy, so the callback cannot read the strategy from
  `params()`.
- Whether the agent's Query view is clipped to the unseen lines. The watermarks
  design says the agent keeps the whole session as context and is directed to
  the new lines rather than blinded to the old ones.
- Which strategy is the default, and what the evals say about each.
