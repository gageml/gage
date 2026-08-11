---
description: Resolve pending Gage issues, then review open issues and close them as completed or skipped.
disable-model-invocation: true
---

Your task is to help the user process Gage issues. There are two phases:

1. _Pending issue resolution_ - reconcile newly written (pending) issues
   against the issues Gage already has, closing duplicates and promoting novel
   issues to open. This phase is automatic except where noted.
2. _Review_ - walk the user through the open issues and help resolve them.

Run phase 1 to completion before starting phase 2.

## Phase 1: Pending issue resolution

Scanners write agent-generated issues with `pending` status. A pending issue is
staged, not yet part of the docket the user works from. Each pending issue is
either a novel finding (promote it to open) or a re-report of something Gage
already has (close it as a duplicate).

You are the sole writer in this phase. Sub-agents analyze and recommend; you
adjudicate and apply. Apply nothing until every sub-agent has returned.

1. Query the pending issues:

   ```sql
   SELECT id, title FROM issue WHERE status = 'pending'
   ```

   If there are none, skip to phase 2.

2. For each pending issue, retrieve its related issues:

   ```sql
   SELECT r.related_id, r.score, i.title, i.status, i.status_reason
   FROM related_issues('<issue_id>') r JOIN issue i ON i.id = r.related_id
   ```

3. A pending issue with no related issues is novel by definition. Do not spawn
   a sub-agent for it; it will be promoted in step 6.

4. For each pending issue that has related issues, spawn one sub-agent. Give
   it the issue ID and its related issue IDs, and instruct it to:
   - Read the full reports: `SELECT report FROM issue_report('<id>')` for the
     subject and each related issue.
   - Judge whether the subject re-reports a condition an existing issue
     already describes. Similar wording alone is not duplication; the
     underlying condition must be the same.
   - Reply with a recommendation in exactly this form, plus a one-paragraph
     rationale:
     - `open` - the subject is novel
     - `duplicate of <id>` - the subject re-reports issue `<id>`. Include a
       `comment` for the surviving issue only when the subject carries
       insight the survivor lacks.
     - `user decision vs <closed id>` - the subject matches a closed issue.
       State what the prior close reason was and whether the evidence
       suggests the condition recurred or the resolution stands.

   Sub-agents read via `mcp__plugin_gage_gage__Query` only. They must not
   write, close, or promote anything.

5. Adjudicate the collected recommendations:
   - Related pending issues see each other, so a duplicate pair can come back
     as two mirrored recommendations (each closing itself against the other,
     or each claiming novelty). Pick one survivor - the richer report, older
     as the tiebreak. The survivor is promoted; the others close against it,
     with their unique insight carried as comments.
   - If two sub-agents disagree about the same pair, decide yourself using
     both rationales.
   - Flatten chains: if A closes against B and B closes against open issue X,
     both close against X. The tool rejects unflattened plans.

6. Apply the automatic batch with a single
   `mcp__plugin_gage_gage__IssuePendingResolve` call covering every pending
   issue except those marked for user decision: `open` resolutions for novel
   issues and promoted survivors, `duplicate` resolutions for the rest.

7. If any issues were marked for user decision, present them to the user as
   one batch, grouped per closed issue, with the prior close reason and the
   sub-agent rationales. For each, the user chooses one of three outcomes:
   - Already resolved - close the pending issue as a duplicate; the closed
     issue stays closed (`duplicate`, no `reopen`)
   - Condition recurred - reopen the closed issue and fold the report into it
     (`duplicate` with `reopen: true`)
   - Stands on its own - promote the pending issue (`open`)

   Apply the decisions with a second `IssuePendingResolve` call.

Do not ask the user for permission to run this phase; only the closed-issue
decisions in step 7 involve them. Briefly report what was resolved: how many
issues were promoted, how many closed as duplicates, and against what.

## Phase 2: Review

Each issue identifies something the user should attend to. A well written issue
describes an underlying condition and provides evidence where available. An
issue may provide steps or guidance for a fix.

Issues are not necessarily related to source code. Issues often touch on
methods, best practices, tool configuration, and so on. Look at each issue in
its own right without pre-judging it.

### Starting steps

1. Use the `mcp__plugin_gage_gage__Query` to select open issues from the `issue`
   table by running `SELECT id, title FROM issue WHERE status = 'open'`. This is
   your working list. Run this query only after phase 1 completes - resolution
   changes which issues are open.

2. Spawn sub-agents instructing each to get the full issue report and summarize
   what it finds using the _issue summary rubric_ below.

3. With the summaries from the sub-agents, triage the list looking for the most
   important issues. Issues that cause the user the most grief or that, when
   fixed, provide the most value are the most important.

4. Present a summary of what you see to the user.

The summary is the most important part of your task. You are introducing the
user to what's in the list. The user likely does not know anything about the
list. Provide a concise, informative overview that highlights what the issues
mean for the user. You've already spent time thinking about how each issue
affects the user. Now's your time to communicate the issues in a way that
connects with the user. The user should be able to think, "Oh yeah, that's
important" or "Yes, that's important to fix". The user should not think, "I have
no idea what that means". Ground the summary for the user.

Let the user know you're prepared to walk through each issue, explain it, and
help them resolve it.

Having presented the summary, ask the user where they'd like to begin.

Other rules related to issue summaries:

- An issue name (`name` col) is not meaningful to the user. Do not show the name
  in a summary list. Refer to an issue by `title`.

### Issue summary rubric

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

### Resolution process

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
