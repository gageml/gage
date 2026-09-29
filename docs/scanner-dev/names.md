# Scanner naming conventions

## Task names

A task name says what the task does. The scanner name says what the task works
on. The two names appear together wherever Gage records a task, so neither one
has to carry the whole meaning. A note written by the `review` task of the
`code-review` scanner has the author `task:code-review:review`. The scan record,
the logs, and the task list show the same pair.

A scanner with one task names it `main`. There is nothing to tell apart, and the
scanner name already says what the task is for.

```rune
pub const SCANNER = #{
    name: "session-retention",
    tasks: #{ main: #{} },
};

pub async fn main() {
    // ...
}
```

A scanner with more than one task names each task for its action. Use a verb.
The `code-review` scanner has a `summarize_projects` task that writes project
summary notes and a `review` task that reads those summaries and writes
findings.

```rune
pub const SCANNER = #{
    name: "code-review",
    tasks: #{
        summarize_projects: #{
            notes: #{ writes: #{ "project-summary.rules": "..." } },
        },
        review: #{
            notes: #{
                wants: ["project-summary.*"],
                writes: #{ "finding.code": "..." },
            },
        },
    },
};
```

Do not name a task for the kind of thing it writes. A task called `note` or
`issue` tells the reader nothing that the task declaration does not already say,
and a task called `issues` in a scanner called `finding-issues` that writes an
issue called `findings` says the same word three times. Name the work instead.
The task that writes those issues is `report`.

Task names are Rune function names. Use lower snake case. A name that reads as a
function call reads well everywhere else.
