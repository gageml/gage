# FILTERS

Some commands show filterable results. These commands have a **--where**
option that takes a _filter expression_ argument. When specified, a
filter expression limits the commands results.

A filter expression consists of one or more logical conditions joined by
logical operators. A condition evaluates a value using an comparison
operator.

Output values are referenced using their column names.

For example, to show runs for a particular operation, a filter
expression might be:

```shell
gage runs --where "operation == train"
```

See **Examples** below for more examples.

Filter expressions must be provided as a single argument, which means
they must be quoted.

## OPERATORS

Filter expressions may use any of the operations listed below.

| Operator | Description              |
| -------- | ------------------------ |
| ==       | Equal to                 |
| !=       | Not equal to             |
| <        | Less than                |
| <=       | Less than or equal to    |
| >        | Greater than             |
| >=       | Greater than or equal to |
| =~       | Matches pattern          |
| !~       | Does not match pattern   |
| +        | Addition                 |
| -        | Subtraction              |
| \*       | Multiplication           |
| /        | Division                 |
| \*\*     | Power                    |
| in       | Value is in a list       |
| not-in   | Is not in a a list       |
| and      | Logical and              |
| or       | Logical or               |

## COMMANDS

Commands may be used in expressions to return calculated values.
Commands must be enclosed in parenthesis to call them.

Filter expressions may use any of the commands listed below.

| Command | Description                          |
| ------- | ------------------------------------ |
| now     | Current day and time                 |
| today   | Time at the start of the current day |

## EXAMPLES

```shell
gage runs --where "status in [completed terminated]"
```

```shell
gage runs --where "operation =~ train"
```

```shell
gage runs --where "starts >= (today)"
```

TODO - more examples
