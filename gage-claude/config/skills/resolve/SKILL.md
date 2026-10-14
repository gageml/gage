---
description:
  Resolve pending Gage issues, then take up an open issue and work it to a
  close.
disable-model-invocation: true
---

Your task is to help the user process Gage issues. There are two phases:

1. _Pending issue resolution_ - reconcile newly written (pending) issues
   against other issues, closing duplicates and promoting novel issues to
   open. This phase does not require user input except where noted below.
2. _Open issue resolution_ - take up one open issue and help the user resolve
   it.

Run phase 1 to completion before starting phase 2.

Several sessions may run this skill at once. An issue a session is working on
has status `resolving`, and the status event that set it carries that
session's ID. Phase 2 claims an issue before working on it and releases or
closes it when done, so two sessions do not take up the same issue.

## Scoped invocation

Arguments to this skill, when present, are issue IDs that restrict scope:

- Phase 1 resolves only the pending issues among them.
- Phase 2 takes up the open issues among them. With one such issue, take it up
  directly. With several, list them and ask which to take up first.
- An ID matching neither a pending nor an open issue is reported to the user
  and otherwise ignored. A resolving issue in scope is reported with its
  holder and the age of its claim; take it up only when the user says so.

Scope restricts which issues are processed, not which issues they are compared
against. Related-issue queries in phase 1 always draw from the full issue set
--- a scoped pending issue can duplicate an issue outside the scope, and must
close against it.

With no arguments, all pending and open issues are in scope.

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

Sub-agents judge clusters of related issues and provide recommendations; they
do not modify issue status. You are the sole writer in this phase and apply
the resolutions with `mcp__plugin_gage_gage__IssuePendingResolve`.

1. Query the pending issues:

   ```sql
   SELECT id, title FROM issue WHERE status = 'pending'
   ```

   If there are none, skip to phase 2.

2. For each pending issue, retrieve its candidate duplicates. `related_issue`
   returns the issues most similar to the subject, best first and at most
   five, drawn from every issue in any status. It retrieves; it does not
   judge.

   ```sql
   SELECT r.related_id, r.score, i.title, i.status, i.status_reason
   FROM related_issue('<issue_id>') r JOIN issue i ON i.id = r.related_id
   ```

   Keep only candidates with a score of 0.15 or higher. In the sessions this
   threshold was calibrated on, every duplicate scored 0.19 or higher against
   its survivor, and every novel issue's best candidate scored below 0.16.

3. A pending issue with no kept candidate is novel. Do not spawn a sub-agent
   for it.

4. Group the remaining pending issues into clusters. A pending issue and its
   kept candidates form one cluster, and two clusters that share a member
   merge. For each cluster, invoke the Agent tool with
   `subagent_type: "gage:pending-issue-reconcile"`, passing every member's
   ID and status in the prompt. The agent's definition contains the full
   instructions and reply contract; the parent supplies only the IDs and
   statuses. Launch every cluster's agent in one message with
   `run_in_background: false`, so the agents run concurrently and their
   results return together in one turn.

5. Collect the recommendations. A cluster's agent names one survivor among
   pending members that describe the same condition and closes the rest
   against it, so no chain or mirrored pair needs flattening. If a
   recommendation names a duplicate as a survivor, or names a survivor outside
   the cluster, decide the pair yourself from the available information.

6. Apply the automatic batch, every pending issue except those marked for user
   decision, in one `IssuePendingResolve` call. The tool takes a `resolutions`
   array with one object per pending issue; it validates the whole plan before
   writing, and orders the writes so each survivor is in its final state before
   a duplicate names it. Each object:
   - A novel issue or a promoted survivor: `{issue, action: "open"}`.
   - A duplicate: `{issue, action: "duplicate", of}` with `of` set to the
     survivor's ID. When the duplicate carries insight the survivor lacks, add
     a `comment` field; it is recorded on the survivor.

   The plan must name each pending issue at most once, and a duplicate's `of`
   must be a survivor the plan promotes or an issue already open, resolving,
   or closed.

7. If any issues are marked for user decision, present each in turn to the user
   as one batch, grouped per closed issue, with the prior close reason and the
   sub-agent rationales. For each, the user chooses one of three outcomes:
   - Already resolved - `{issue, action: "duplicate", of}` against the closed
     issue; the closed issue stays closed
   - Condition recurred - `{issue, action: "duplicate", of, reopen: true}`
     against the closed issue, with a `comment` carrying the report's insight;
     `reopen` returns the survivor to open before the close
   - Stands on its own - `{issue, action: "open"}`

   Apply the final decisions in a second `IssuePendingResolve` call.

8. Briefly report what was resolved: how many issues were promoted, how many
   closed as duplicates, and against what.

## Phase 2: Open issue resolution

Each open issue identifies something the user should attend to. A well written
issue describes an underlying condition and provides evidence where available.
An issue may provide steps or guidance for a fix.

Issues are not necessarily related to source code. Issues often touch on
methods, best practices, tool configuration, and so on. Look at each issue in
its own right without pre-judging it.

### Phase 2a: Choose an issue

1. Query the open issues and the issues other sessions are resolving. Run
   these queries only after phase 1 completes - resolution changes which
   issues are open.

   ```sql
   SELECT id, title, created FROM issue WHERE status = 'open' ORDER BY created
   ```

   ```sql
   WITH claims AS (
     SELECT issue_id, timestamp, message,
            row_number() OVER (PARTITION BY issue_id ORDER BY timestamp DESC) AS rn
     FROM issue_event
     WHERE event = 'status' AND to_status = 'resolving'
   )
   SELECT i.id, i.title, c.timestamp AS claimed, c.message AS holder
   FROM issue i JOIN claims c ON c.issue_id = i.id AND c.rn = 1
   WHERE i.status = 'resolving'
   ```

2. Present the open issues using the table format below, oldest first. Do not
   use issue names in your summary --- these are internal values and do not
   have meaning to a user. Use the short issue ID (first 8 chars) and the
   issue title.

   ```
   | Issue      | Title   | Age   |
   | ---------- | ------- | ----- |
   | {short_id} | {title} | {age} |
   ```

   Below the table, list any resolving issues with their holder and the age of
   the claim. A claim older than 24 hours is abandoned: say so, and offer to
   take the issue up. Do not take up a younger claim unless the user says so.

3. With one open issue, say so and take it up. With more than one, ask:

   > Would you like to start with a specific issue or should I take the first
   > in order?

### Phase 2b: Resolve the issue

Gage issues are user-directed and therefore it's imperative that the user
understand what is being presented. Do not assume the user knows what an issue
is based on its ID (meaningless) or title. Your job is to foster understanding
first and only then to resolve issues with user direction.

**Claim the issue**

Before any other work on the issue, claim it with
`mcp__plugin_gage_gage__IssueUpdate`, `status=resolving`, and the session ID
from `$CLAUDE_CODE_SESSION_ID` as `message`. A refusal means another session
claimed it first: report that and return to phase 2a.

**Verify that the issue is valid**

Read the full report with `SELECT report FROM issue_report('<issue_id>')`.
Then verify the current state of affairs. The issue may already have been
addressed. The state of affairs may have changed. Do not take anything for
granted. If you proceed without confirming the current state, you risk wasting
time and making matters worse.

You are free to investigate further using `Query` to read referenced notes and
session messages. If the issue related to project files, use tools to get the
information you need before presenting your findings to the user.

**Present the issue**

Use the following template when presenting the issue:

```
### `{issue_short_id}` - {title}

Applies to `{project_1}`, `{project_2}`

{short_summary}

**Status** - {status}

**Recommendation** - {recommendation}
```

- `issue_short_id` - First 8 chars of the issue ID
- `title` - Issue title
- `project_1`, `project_2` - List of projects the issue applies to. Use
  `SELECT DISTINCT s.project FROM session_issue si JOIN session s ON s.id = si.session_id WHERE si.issue_id = '<issue_id>'`
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
    closed as "wontfix" (i.e. not implemented) with an explanation

Ask the user how they would like to proceed or if they need more information.

Support the user in understanding what the issue says, what it means in terms of
benefit (if fixed) or cost (if not fixed). If there are trade offs to consider,
present those clearly so the user understands what's at stake.

**Resolve the issue**

After your investigation, if you determine that the issue is invalid or no
longer applicable, inform the user and recommend that the issue be closed as
wontfix (i.e. closed with "wontfix" reason -- see below).

If the issue is valid, get the user's explicit approval before resolving any
issues. Do not make any changes without approval from the user.

Present any recommended options along with the option to defer or close the
issue as wontfix.

Before closing, ask the user to confirm a fix only when they can verify
something the tests do not already cover. If their check would just repeat the
tests, close without asking. User commentary about the issue, context about a
failure mode, or agreement with my analysis is not a directive to close.

Use `mcp__plugin_gage_gage__IssueUpdate` with `status=closed` and
`status_reason=completed` to close an issue that's been verified as fixed.

If the user decides to not fix the issue, close it with `status=closed` and
`status_reason=wontfix`.

**Release the issue**

A close ends the claim. If the user defers the issue, or stops work on it
without closing, release it with `status=open` so another session can take it
up. After a close or a release, return to phase 2a and present the remaining
open issues.
