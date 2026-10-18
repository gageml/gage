---
description:
  Resolve pending Gage issues, then take up an open issue and work it to a
  close.
disable-model-invocation: true
---

Your task is to help the user process Gage issues. There are three phases:

**Phase 1 - Reconcile pending issues.** Reconcile pending issues against other
issues, closing duplicates and promoting novel issues to open. This phase does
not require user input except where noted below.

**Phase 2 - Free abandoned issues**. Run `IssuesFree` tool to free assigned
issues that have been abandoned. Issue assignment is described below in
**Concurrency**. This phase does not require user input.

**Phase 3 - Resolve issues.** Take up open issues and work with the user to
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

## Phase 1: Reconcile pending issue

**Background**

Scans may write issues that have already been reported, in part or whole. To
indicate uncertainty about the veracity of an issue, a scanner writes it with
`pending` status.

A pending issue must first be reconciled with other issues as novel or as a
duplicate.

Phase 1 performs this reconciliation for all pending issues.

**Guidelines**

Do not ask the user for permission to run phase 1. Only ask the user for input
as per step 8 below.

Sub-agents judge groups of related issues and provide recommendations. They do
not modify issue status. You are the sole writer in this phase and apply the
resolutions with `mcp__plugin_gage_gage__IssuePendingResolve`.

**Steps**

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

## Phase 3: Resolve issues

Each open issue identifies something the user should attend to. A well written
issue describes an underlying condition that needs user attention and provides
evidence where available. An issue may provide steps or guidance for a fix.

Issues are not necessarily related to source code. Issues often touch on
methods, best practices, tool configuration, and so on. Look at each issue in
its own right without pre-judging it.

### 3a: Present open issues

In this phase you show the user the list of open issues and get user direction
on how to proceed.

**Steps**

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

3. If there is more than one open issue, ask the user:

   > Would you like to start with a specific issue or should I take each issue
   > in order?

In phases 3b and 3c, you work with the user to resolve issues, one at a time.

### 3b: Take up an issue

Before resolving an issue it must be assigned to you via the
`mcp__plugin_gage_gage__IssueAssign` tool. This ensures that other sessions do
not also take up the issue and conflict with your work.

If you know the issue ID to take up (e.g. the user has specified a specific
issue) use the `issue` input to request that issue. If the issue it not already
assigned, it will be assigned to you. If the issue is already assigned, the tool
will tell you which session is working on it. If the issue appears abandoned or
the user want to override the assignment, use `force` in a second call to take
up the issue.

If you are instructed to take each issue in order, present the list of issues in
order using the `pool` input. The tool assigned the next free issue. If there
are no free issues, the tool says so.

### Phase 3c: Resolve the issue

Proceed only once the issue is assigned to you via
`mcp__plugin_gage_gage__IssueAssign`.

**Guidelines**

Gage issues are user-directed and it's important the user understand what is
being presented. Do not assume the user knows what an issue is based on its
title. Your job is to foster understanding first and only then to resolve issues
with user direction.

Issue reports may be spurious, contain errors, or be stale. Present current,
verified status before getting user input or proceeding with a fix.

**Steps**

1. Investigate the issue.

   Read the full report with `SELECT report FROM issue_report('<issue_id>')`.

   Investigate the issue to gather current information. In your investigation,
   look for information like:

   - Does the issue make a valid claim that can be fixed?
   - Can you confirm the issue still exists?
   - Has any work already been done to resolve the issue?

   Use `mcp__plugin_gage_gage__Query` to read referenced notes and session
   messages. If the issue relates to project files, use tools to get the
   information you need before presenting findings to the user.

   The presence of evidence does not prove an issue is valid or current.
   Investigate the underlying claims using cited evidence only as a guide.

   The issue report may be stale. It may have been fixed or worked on since
   being reported.

   A claim you did not check is unverified. Present it as `Unverified` with the
   reason.

2. Present the issue.

   Use the following template to present the issue:

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

   - `project_1`, `project_2` - List of projects the issue applies to. Use:

     ```sql
     SELECT DISTINCT s.project FROM session_issue si
     JOIN session s ON s.id = si.session_id
     WHERE si.issue_id = '<issue_id>'
     ```

   - `short_summary` - One or two sentences introducing the issue to the user.
     Assume the user has no context for the issue and needs it explained in
     basic terms.

   - `status` - One of:
     - `Verified` with what you checked and what it showed
     - `Unverified` with the reason the claim could not be checked
     - `No longer applicable` with a summary of what may have changed since the
       issue was opened
     - `Invalid` or `Likely invalid` or `Possibly invalid` with a summary of how
       the issue as reported is not accurate

   - `implication` - One or two sentences describing what the issue means for
     the user. This is where you answer "What does this mean to me?" and is the
     most important summary of the issue presentation.

   - `recommendation` - How to proceed
     - If the issue is verified, recommend a fix or approach to working toward a
       fix (e.g. further research, etc.)
     - If the issue is no longer applicable or invalid, recommend the issue be
       closed as "wontfix" (i.e. not implemented) with an explanation

   Ask the user how they would like to proceed or if they need more information.

   Support the user in understanding what the issue says, what it means in terms
   of benefit (if fixed) or cost (if not fixed). If there are trade offs to
   consider, present those clearly so the user understands what's at stake.

3. Work with the user to resolve the issue

   Get the user's explicit approval before resolving any issues. Do not make any
   changes without approval from the user.

   Before closing, ask the user to confirm a fix.

   Exceptions to asking for confirmation:

   - Tests - do not ask the user to run tests --- if passing tests confirm the
     fix, close the issue directly

   A `completed` close rests on a check that showed the fix: a test that passed,
   a command whose output you read, or the user's confirmation. An explanation
   of why the fix should work is not a check. The close message names the check.

   Use `mcp__plugin_gage_gage__IssueUpdate` with `status=closed` and
   `status_reason=completed` to close an issue that's been verified as fixed.
   `message` states what was done; the tool refuses a close without one.

   If the user decides to not fix the issue, close it with `status=closed` and
   `status_reason=wontfix`. `message` states why the issue will not be
   addressed.

4. Release the issue

   If the user defers the issue, or stops work on it without closing, release it
   with `mcp__plugin_gage_gage__IssueUpdate` and `status=open` so another
   session can take it up. After a close or a release, return to phase 3a.
