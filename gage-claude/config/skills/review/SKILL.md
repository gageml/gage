---
description:
  Resolve pending Gage issues, then review open issues and close them as
  completed or skipped.
disable-model-invocation: true
---

Your task is to help the user process Gage issues. There are two phases:

1. _Pending issue resolution_ - reconcile newly written (pending) issues against
   other issues, closing duplicates and promoting novel issues to open. This
   phase does not require user input except where noted below.
2. _Review_ - walk the user through open issues and help resolve them.

Run phase 1 to completion before starting phase 2.

## Phase 1: Pending issue resolution

**Background**

Session re-scans may generate issues that have already been reported. To
indicate an issue should be first resolved as a potential duplicate, a scanner
can write the new issue with a `pending` status.

Pending issues must first be resolved as novel or as duplicates of existing
issues.

**Instructions**

Do not ask the user for permission to run this phase. Only ask the user for
input as per step 7 below.

Use a sub-agent for each pending issue to resolve it using the steps below. Note
that sub-agents provide recommendations and do not modify issue status. You are
the sole writer in this phase and will resolve any conflicts across sub-agents
before using the `IssuePendingResolve` tool to finally resolve pending issues.

1. Query the pending issues:

   ```sql
   SELECT id, title FROM issue WHERE status = 'pending'
   ```

   If there are none, skip to phase 2.

2. For each pending issue, retrieve its related issues:

   ```sql
   SELECT r.related_id, r.score, i.title, i.status, i.status_reason
   FROM related_issue('<issue_id>') r JOIN issue i ON i.id = r.related_id
   ```

3. A pending issue with no related issues is novel by definition. Do not spawn a
   sub-agent for it.

4. For each pending issue with related issues, spawn one sub-agent. Give it the
   issue ID and its related issue IDs, and instruct it to:
   - Read the full reports: `SELECT report FROM issue_report('<id>')` for the
     subject and each related issue.
   - Judge whether the subject re-reports a condition an existing issue already
     describes. Similar wording alone is not duplication; the underlying
     condition must be the same.
   - Reply with a recommendation in exactly this form, plus a one-paragraph
     rationale:
     - `open` - the subject is novel
     - `duplicate of <id>` - the subject re-reports issue `<id>`. Include a
       `comment` for the surviving issue only when the subject carries insight
       the survivor lacks.
     - `user decision vs <closed id>` - the subject matches a closed issue.
       State what the prior close reason was and whether the evidence suggests
       the condition recurred or the resolution stands.

   Sub-agents read via `mcp__plugin_gage_gage__Query` only. They do not write,
   close, or promote anything.

5. Adjudicate the collected recommendations:
   - Pending issues may be related to other pending issues (e.g. multiple scans
     without resolution) --- a duplicate pair can come back as two mirrored
     recommendations. Pick one survivor according to issue report quality. The
     survivor is promoted. The others close against it, with their unique
     insight carried as comments.
   - If two sub-agents disagree about the same pair, decide yourself using the
     available information.
   - Flatten chains: if A closes against B and B closes against open issue X,
     both close against X. The `IssuePendingResolve` tool rejects unflattened
     plans.

6. Apply the automatic batch with a single
   `mcp__plugin_gage_gage__IssuePendingResolve` call covering every pending
   issue except those marked for user decision: `open` resolutions for novel
   issues and promoted survivors, `duplicate` resolutions for the rest.

7. If any issues are marked for user decision, present each in turn to the user
   as one batch, grouped per closed issue, with the prior close reason and the
   sub-agent rationales. For each, the user chooses one of three outcomes:
   - Already resolved - close the pending issue as a duplicate; the closed issue
     stays closed (`duplicate`, no `reopen`)
   - Condition recurred - reopen the closed issue and fold the report into it
     (`duplicate` with `reopen: true`)
   - Stands on its own - promote the pending issue (`open`)

   Apply the final decisions with a second `IssuePendingResolve` call.

8. Briefly report what was resolved: how many issues were promoted, how many
   closed as duplicates, and against what.

## Phase 2: Review

Each open issue identifies something the user should attend to. A well written
issue describes an underlying condition and provides evidence where available.
An issue may provide steps or guidance for a fix.

Issues are not necessarily related to source code. Issues often touch on
methods, best practices, tool configuration, and so on. Look at each issue in
its own right without pre-judging it.

### Phase 2a: Present a summary

1. Use the `mcp__plugin_gage_gage__Query` to select open issues from the `issue`
   table by running `SELECT id, title FROM issue WHERE status = 'open'`. This is
   your working list. Run this query only after phase 1 completes - resolution
   changes which issues are open.

2. Spawn sub-agents instructing each to get the full issue report and summarize
   what it finds using the _issue summary rubric_ below.

3. With the summaries from the sub-agents, triage the list looking for the most
   important issues and list those first. Issues that cause the user the most
   grief or that, when fixed, provide the most value are the most important.

4. Present a summary of what you see to the user using the table format below.
   Do not use issue names in your summary --- these are internal values and do
   not have meaning to a user. Use the short issue ID (first 8 chars) and the
   issue title.

Issue summary table format:

```
| Issue      | Title   | Priority          |
| ---------- | ------- | ----------------- |
| {short_id} | {title} | {high,medium,low} |
```

Priority is your rollup of the sub-agent's user benefit, user pain addressed,
fix ease, and fix risk for that issue. Use high, medium, or low according to
your own judgement from the sub-agent results.

Having presented the summary, get direction from the user. If there is more than
one issue to address, use this statement:

> Would you like to start with a specific issue or should I present each in
> order?

**Sub-agent summary rubric**

When spawning a sub-agent to summarize an issue, give the sub-agent an issue ID
and ask it to run this query:

```sql
SELECT report FROM issue_report('<issue_id>')
```

This returns the detail report for the issue.

The sub-agent should evaluate the issue --- assuming the issue to be true and
accurate --- using this rubric:

- `user benefit` - If solved, how do things improve for the user? What benefit
  would the user enjoy?
- `user pain addressed` - If left unsolved, what cost does the user incur? What
  pain does the user face?
- `fix ease` - If the issue proposes a fix, how straight forward is the fix to
  apply?
- `fix risk` - If the issue proposes a fix, what risk does it present to the
  user? If something goes wrong, what could it cost the user?

The sub-agent should include the issue report in its reply along with its
answers to the rubric questions.

### Phase 2b: Resolve issues

After presenting the summary above, your job is to resolve the issue as per the
user's direction. Each issue should be resolved in series unless it's clear a
set of issues are related and can be worked on as a single unit.

Gage issues are user-directed and therefore it's imperative that the user
understand what is being presented. Do not assume the user knows what an issue
is based on its ID (meaningless) or title. Your job is to foster understanding
first and only then to resolve issues with user direction.

**Verify that the issue is valid**

When you take up an issue, first verify the current state of affairs. The issue
may already have been addressed. The state of affairs may have changed. Do not
take anything for granted. If you proceed without confirming the current state,
you risk wasting time and making matters worse.

You are free to investigate further using `Query` to read referenced notes and
session messages. If the issue related to project files, use tools to get the
information you need before presenting your findings to the user.

**Present the issue**

Use the following template when presenting the issue:

```
### {title}

{short_summary}

**Status** - {status}

**Recommendation** - {recommendation}
```

- `title` - Issue title
- `short_summary` - One or two sentences introducing the issue to the user.
  Assume the user has no context for the issue and needs it explained in basic
  terms.
- `status` - One of:
  - `Verified` with an explanation of your confirmed finding
  - `No longer applicable` with an explanation of what may have changed since
    the issue was opened
  - `Invalid` or `Likely invalid` or `Possibly invalid` with an explanation of
    how the issue as reported is not accurate
- `recommendation` - How to proceed
  - If the issue is verified, recommend a fix or approach to working toward a
    fix (e.g. further research, etc.)
  - If the issue is no longer applicable or invalid, recommend the issue be
    closed as "skipped" (i.e. not implemented) with an explanation

Ask the user how they would like to proceed or if they need more information.

Support the user in understanding what the issue says, what it means in terms of
benefit (if fixed) or cost (if not fixed). If there are trade offs to consider,
present those clearly so the user understands what's at stake. Draw on the
sub-agent's rubric answers from Phase 2a (benefit, pain, ease, risk) to ground
this discussion.

**Resolve the issue**

After your investigation, if you determine that the issue is invalid or no
longer applicable, inform the user and recommend that the issue be skipped (i.e.
closed with "skipped" reason -- see below).

If the issue is valid, get the user's explicit approval before resolving any
issues. Do not make any changes without approval from the user.

Present any recommended options along with the option to defer or close the
issue as skipped.

Always confirm (or ask the user to confirm) fixes before closing the issue. User
commentary about the issue, context about a failure mode, or agreement with my
analysis is not a directive to close.

Use `mcp__plugin_gage_gage__IssueUpdate` with `status=closed` and
`status_reason=completed` to close an issue that's been verified as fixed.

If the user decides to not fix the issue, close it with `status=closed` and
`status_reason=skipped`.
