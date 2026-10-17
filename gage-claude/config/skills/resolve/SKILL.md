---
description:
  Resolve pending Gage issues, then take up an open issue and work it to a
  close.
disable-model-invocation: true
---

Your task is to help the user process Gage issues. There are three phases:

1. **Pending issue reconciliation.** Reconcile pending issues against other
   issues, closing duplicates and promoting novel issues to open. This phase
   does not require user input except where noted below.

2. **Free abandoned issues**. Run `IssuesFree` tool to free assigned issues that
   have been abandoned. Issue assignment is described below in **Concurrency**.
   This phase does not require user input.

3. **Open issue resolution.** Take up open issues and work with the user to
   resolve them.

Run phases 1 and 2 to completion before starting phase 3.

Do not use the term "phase" in status messages or your correspondence with the
user. Use these terms instead:

- Phase 1 -> "reconcile pending issues"
- Phase 2 -> "free abandoned issues"
- Phase 3 -> "resolve open issues"

## Concurrency

Several sessions may run this skill at once. An issue a session is working on
has status `assigned`. The status event shows the timestamp and the session
working on the issue.

A session is assigned an issue when it calls `IssueAssign`.

In this way multiple sessions work on open issues concurrently without
duplicating work or conflict.

## Scoped invocation

Arguments to this skill, when present, are issue IDs that restrict scope as
follows:

- Phase 1 resolves only the pending issues among them.

- Phase 2 frees _all_ abandoned issues as a side-effect of running this skill.
  This is an intentional deviation from phase 1 and phase 3, which are scoped.
  Phase 2 acts as a lazy sweep over assigned issues.

- Phase 3 takes up open issues among them. With one such issue, take it up
  directly. With several, list them and ask which to take up first.

Scope restricts which issues are processed, not which issues they are compared
against. Related-issue queries in phase 1 draw from the full issue set. A scoped
pending issue can duplicate an issue outside the scope, and must close against
it.

With no arguments, all pending and open issues are in scope.

## Phase 1: Pending issue reconciliation

**Background**

Scans may write issues that have already been reported, in part or whole. To
indicate uncertainty about the veracity of an issue, a scanner writes it with
`pending` status.

A pending issue must first be reconciled with other issues as novel or as a
duplicate.

Phase 1 performs this reconciliation for all pending issues.

**Instructions**

Do not ask the user for permission to run phase 1. Only ask the user for input
as per step 8 below.

Sub-agents judge groups of related issues and provide recommendations; they do
not modify issue status. You are the sole writer in this phase and apply the
resolutions with `mcp__plugin_gage_gage__IssuePendingResolve`.

1. Query the pending issues:

   ```sql
   SELECT id, title FROM issue WHERE status = 'pending'
   ```

   If there are no rows, skip to phase 2.

2. For each pending issue, list duplicate candidates using the `related_issue`
   TVF. It returns issues similar enough to the subject to be duplicate
   candidates, best first, drawn from every issue in any status.

   ```sql
   SELECT r.related_id, i.title, i.status, i.status_reason
   FROM related_issue('<issue_id>') r JOIN issue i ON i.id = r.related_id
   ```

3. If `related_issue` returns 0 rows for a pending issue, designate it for
   `open` status (to be applied in step 7 below)

4. Create groups of potentially related issues. To form a group, combine a
   pending issue with its related issue candidates from step 2. Merge groups
   that share an issue. After merging, no issue appears in two groups.

5. For each group, use the Agent tool with
   `subagent_type: "gage:pending-issue-reconcile"` and pass the group's list of
   issues, each issue with its respective status. Launch every agent in one
   message with `run_in_background: false` so the agents run concurrently and
   their results return together in one turn.

6. Each agent returns a JSON object:

   ```json
   {
     "summary": "...",
     "issues": {
       "<id>": { "action": "new" },
       "<id>": { "action": "duplicate", "related": "<id>", "comment": "..." },
       "<id>": { "action": "reopen", "related": "<id>", "comment": "..." }
     }
   }
   ```

   `summary` is the rationale for the group. Use it to provide context to the
   user for `reopen` cases or to answer questions the user may have.

   `issues` provides a recommended `action` for every pending issue in the
   group:
   - `new` - issue is new and should be opened
   - `duplicate` - issue is a duplicate of non-closed issue `related`
   - `reopen` - issue is a duplicate of closed issue `related` and the user
     needs to help decide its status

   If provided, `comment` is the `comment` value for the issue in the
   `IssuePendingResolve` call --- information the issue adds that `related`
   lacks.

7. Apply the automatic batch, every `new` and `duplicate` issue, in one
   `IssuePendingResolve` call. The tool takes a `resolutions` array with one
   object per pending issue; it validates the whole plan before writing, and
   orders the writes so each surviving issue is in its final state before a
   duplicate names it. Each object:
   - `new` - `{issue, action: "open"}`
   - `duplicate` -
     `{issue, action: "duplicate", duplicate_of: related, comment}`

   The plan must name each pending issue at most once, and a duplicate's
   `duplicate_of` must be an issue the plan promotes or an issue already open,
   assigned, or closed.

8. Present the `reopen` issues to the user as one batch, grouped by `related`,
   with the prior close reason and the agent's `summary`. For each, the user
   chooses one of three outcomes:
   - Already resolved - `{issue, action: "duplicate", duplicate_of: related}`;
     the closed issue stays closed
   - Condition recurred -
     `{issue, action: "duplicate", duplicate_of: related, reopen: true, comment}`;
     `reopen` returns the closed issue to open before the close
   - Stands on its own - `{issue, action: "open"}`

   Apply the final decisions in a second `IssuePendingResolve` call.

9. Briefly report what was resolved: how many issues were promoted, how many
   closed as duplicates, and against what.

## Phase 2: Free abandoned issues

Run `mcp__plugin_gage_gage__IssuesFree` with no inputs to free abandoned issues
in preparation for phase 3. The tool determines when an assigned issue no longer
has an active session and changes its status to `open`.

## Phase 3: Open issue resolution

Each open issue identifies something the user should attend to. A well written
issue describes an underlying condition and provides evidence where available.
An issue may provide steps or guidance for a fix.

Issues are not necessarily related to source code. Issues often touch on
methods, best practices, tool configuration, and so on. Look at each issue in
its own right without pre-judging it.

### Phase 3a: Choose an issue

1. List the open issues. Run this query only after phase 2 completes -
   resolution changes which issues are open.

   ```sql
   SELECT id, title, age(created) AS age
   FROM issue WHERE status = 'open' ORDER BY created DESC
   ```

   These issues, in this order, are the pool for `IssueAssign` until the next
   listing. An issue opened after this listing is not in the pool.

2. Present the open issues using the table format below. Do not use issue names
   in your summary. Issue names are internal values and do not have meaning to a
   user. Use the short issue ID (first 8 chars) and the issue title.

   ```
   | Issue      | Title   | Age   |
   | ---------- | ------- | ----- |
   | {short_id} | {title} | {age} |
   ```

3. With one open issue, proceed to phase 3b with it. With more than one, ask:

   > Would you like to start with a specific issue or should I take the first in
   > order?

For the next phase you work with the user to resolve each issue in turn.

### Phase 3b: Resolve an issue

Gage issues are user-directed and it's important the user understand what is
being presented. Do not assume the user knows what an issue is based on its
title. Your job is to foster understanding first and only then to resolve issues
with user direction.

**Take up the issue**

As per **Concurrency** above, you may be running alongside other sessions
working on the same set of issues. To resolve an issue it must be assigned to
you. Once assigned, the issue will not be assigned to another session.

Use `mcp__plugin_gage_gage__IssueAssign` to request assignment. The tool takes
one of two inputs:

- `pool` - a list of issue IDs in order. The tool assigns you the first one that
  is open. Use this to take up the next issue from the listing.

- `issue` - one issue ID. The tool assigns you that issue if it is open. Use
  this to take up a specific issue. If the issue is assigned to another session
  the tool refuses, unless `force` is set, in which case it reassigns the issue
  to you. Use `force` only when the user directs you to.

If the tool reports that no issue was assigned, return to phase 3a.

**Verify that the issue is valid**

Once the issue has been assigned to you, read the full report with
`SELECT report FROM issue_report('<issue_id>')`. Verify the current state of
affairs. The issue may already have been addressed but not updated. The state of
affairs may have changed. Do not take anything for granted. If you proceed
without confirming the current state, you risk wasting time and making things
worse.

Investigate further as needed using `Query` to read referenced notes and session
messages. If the issue relates to project files, use tools to get the
information you need before presenting findings to the user.

**Present the issue**

Use the following template when presenting the issue:

```
### `{issue_short_id}` - {title}

Applies to `{project_1}`, `{project_2}`

{short_summary}

**Status** - {status}

**Implication** - {implication}

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
  - `Verified` with a summary of your confirmed finding
  - `No longer applicable` with a summary of what may have changed since the
    issue was opened
  - `Invalid` or `Likely invalid` or `Possibly invalid` with a summary of how
    the issue as reported is not accurate

- `implication` - One or two sentences describing what the issue means for the
  user. This is where you answer "What does this mean to me?" and is the most
  important summary of the issue presentation.

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

Get the user's explicit approval before resolving any issues. Do not make any
changes without approval from the user.

Before closing, ask the user to confirm a fix.

Exceptions to asking for confirmation:

- Tests - do not ask the user to run tests --- if passing tests confirm the fix,
  close the issue directly

Use `mcp__plugin_gage_gage__IssueUpdate` with `status=closed` and
`status_reason=completed` to close an issue that's been verified as fixed.

If the user decides to not fix the issue, close it with `status=closed` and
`status_reason=wontfix`.

**Release the issue**

If the user defers the issue, or stops work on it without closing, release it
with `mcp__plugin_gage_gage__IssueUpdate` and `status=open` so another session
can take it up. After a close or a release, return to phase 3a.
