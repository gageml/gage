+++
name = "IssueAssign"

[parameters.pool]
type = "array"
required = false
description = "Issue IDs in order. The first open one is assigned"

[parameters.pool.items]
type = "string"

[parameters.issue]
type = "string"
required = false
description = "One issue ID or unique prefix to assign"

[parameters.force]
type = "boolean"
required = false
description = "With issue: reassign it when another session holds it"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to have an open issue assigned to this session before working on
it. The issue's status becomes `assigned` and the status event records
this session. Another session cannot take an assigned issue, so
sessions resolving issues at the same time do not work the same one.

Give exactly one of `pool` or `issue`.

`pool` is a list of issue IDs in order. The first one that is open is
assigned; issues in the list that are assigned, pending, or closed are
passed over. When none is open the result says so and nothing is
written.

`issue` names one issue. It is assigned when open. When another
session holds it the call is refused unless `force` is set, which
reassigns it to this session. A pending or closed issue is refused.

Releasing an assigned issue is `IssueUpdate` with `status=open`;
closing it ends the assignment.
