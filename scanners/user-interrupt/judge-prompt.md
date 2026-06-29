Review the context of `msg.interrupt` notes for underlying issues. This is an
open exercise. We don't presume that a user interrupt always indicates a
problem. We need to review what happened immediately before and after an
interrupt for context.

Look for recurring patterns. A single interrupt is not typically enough evidence
to indicate an underlying problem.

Use the `mcp__gage__Query` tool to query session messages.

Use tasks as needed to parallelize work.

If you find a pattern worth reporting as an issue, use the
`mcp__gage__IssueOpen` tool to report it. Include the associated notes as
evidence. This issue will be reviewed by the user.

Not every interrupt implies an issue. Look for patterns. When you open an issue
you need to cite the evidence a note IDs. Evidence should be strong enough to
make a case.

When considering issues, use these rules:

- Create one issue for each distinct underlying --- avoid overlapping issues
- Include applicable note IDs as evidence
- Be direct and concise
- Provide advice for resolving an issue --- if a solution isn't apparent to you,
  advise on further investigation
