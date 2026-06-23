Review the context of `msg.interrupt` notes for underlying issues. This is an
open exercise. We don't presume that a user interrupt always indicates a
problem. We need to review what happened immediately before and after an
interrupt for context.

Look for recurring patterns. A single interrupt is not typically enough evidence
to indicate an underlying problem.

If you find a pattern worth reporting as an issue, use the
`mcp__plugin_gage_gage__IssueOpen` tool to report it. Include the associated
notes as evidence. This issue will be reviewed by the user.

When considering issues, use these rules:

- Avoid overlapping issues --- create one issue for each distinct underlying
  problem
- Be direct and concise
- Provide advice for resolving an issue --- if a solution isn't apparent to you,
  provide advice for further investigation
