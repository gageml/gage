---
description: Review Gage issues and close them as completed or skipped.
disable-model-invocation: true
---

Your task is to help the user process open Gage issues.

Each issue identifies something the user should attend to. A well written issue
describes an underlying condition and provides evidence where available. An
issue may provide steps or guidance for a fix.

Issues are not necessarily related to source code. Issues often touch on
methods, best practices, tool configuration, and so on. Look at each issue in
its own right without pre-judging it.

## Starting steps

1. Use the `mcp__plugin_gage_gage__Query` to select open issues from the `issue`
   table by runnint `SELECT id, title FROM issue WHERE status = 'open'`. This is
   your working list.

2. Spawn sub-agents instructing each to get the full issue report and summarize
   what it finds using the _issue summary rubric_ below.

3. With the summaries from the sub-agents, triage the list looking for the most
   important issues. Issues that cause the user the most grief or that, when
   fixed, provide the most value are the most important.

4. Present a summary of what you see to the user.

The summary is the most important part of your task. You are introducing the
user to what's in the list. The user likely does not know anything about the
list. Provide a consise, informative overview that highlights what the issues
mean for the user. You've aleady spend time thinking about how each issue
affects the user. Now's your time to communicate the issues in a way that
connects with the user. The user should be able to think, "Oh yeah, that's
important" or "Yes, that's important to fix". The user should not think, "I have
no idea what that means". Ground the summary for the user.

Let the user know you're prepated to walk through each issue, explain it, and
help them resolve it.

Having presented the summary, ask the user where they'd like to begin.

Other rules related to issue summaries:

- An issue name (`name` col) is not meaningful to the user. Do not show the name
  in a summary list. Refer to an issue by `title`.

## Issue summary rubric

Give each sub-agent an issue ID and ask it to run this query:

```sql
SELECT report FROM issue_report('<issue_id>')
```

This returns the detail report for the issue.

The sub-agent should evaluate the issue --- assuming the issue to be true and
accurate --- using this rubric:

- `user benefit` - If solved, how do thing improve for the user? What benefit
  would the user enjoy?
- `user pain addressed` - If left unsolved, what cost does the user incur? What
  pain does the user face?
- `fix ease` - According to the issue fix advice, how straight forward is the
  fix to apply?
- `fix risk` - What risk does the proposed fix present to the user? If something
  goes wrong, what could it cost the user?

The sub-agent should include the issue report in its reply along with its
answers to the rubric questions.

## Resolution process

When working on an issue, verify the current state of affairs. The issue may
already have been addressed. The state of affairs may have changed. Do not take
anything for granted. If you proceed without confirming the current state, you
risk wasting time and making matters worse.

You are free to investigate further using `Query` to read referenced notes and
session messages.

Do not attempt to resolve more than one issue at a time. This is a step-wise
process. This is not a push-button, "Claude does everything for you" process.

Support the user in understanding what the issue says, what it means in terms of
benefit (if fixed) or cost (if not fixed). If there are trade offs to consider,
present those clearly so the user understands what's at stake.

Get the user's explicit approval before resolving any issues. Do not make any
changes without getting approval from the user. You will be strictly graded on
this.

Always verify (or ask the user to verify) fixes before closing the issue.

Use `mcp__plugin_gage_gage__IssueClose` to close an issue that's been verified as
fixed.

If the user decides to not fix the issue, close the issue with the
`reason=skipped`. Otherwise the issue will be closed as `completed`.
