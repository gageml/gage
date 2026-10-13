# Scanner naming conventions

## Task names

A task name says what the task does. The scanner name says what the task works
on. The two names appear together wherever Gage records a task, so neither one
has to carry the whole meaning. A note written by the `scan_sessions` task of
the `code-review` scanner has the author `task:code-review:scan_sessions`. The
scan record, the logs, and the task list show the same pair.

A scanner with one task names it `main`. There is nothing to tell apart, and the
scanner name already says what the task is for.

```rune
pub const SCANNER = #{
    name: "hello",
    tasks: #{ main: #{} },
};

pub async fn main() {
    // ...
}
```

A scanner with more than one task names each task for its action. Use a verb.
The `code-review` scanner has an `attach_project_files` task that attaches
each project's rule and manifest files, a `summarize_projects` task that
writes project summary notes, and a `scan_sessions` task that reads those
summaries and writes findings.

```rune
pub const SCANNER = #{
    name: "code-review",
    tasks: #{
        attach_project_files: #{
            attaches: ["project-rule-files", "project-stack-files"],
        },
        summarize_projects: #{
            attachments: #{ wants: ["project-rule-files", "project-stack-files"] },
            notes: #{ writes: #{ "rules[project-summary]": "..." } },
        },
        scan_sessions: #{
            notes: #{
                wants: ["rules[project-summary]"],
                writes: #{ "code-review[finding]": "..." },
            },
        },
    },
};
```

Do not name a task for the kind of thing it writes. A task called `note` or
`issue` tells the reader nothing that the task declaration does not already say,
and a task called `issues` in a scanner called `findings` that writes an issue
called `findings` says the same word three times. Name the work instead. The
task that writes those issues is `review_findings`.

Task names are Rune function names. Use lower snake case. A name that reads as a
function call reads well everywhere else.

## Note names

A note name is `<what>[<kind>]`. The bracketed kind groups names that
downstream tasks select together, and the part before it says what the note
is about. The `general` scanner writes `general[finding]` and the
`code-review` scanner writes `code-review[finding]`; the `findings` scanner
wants `*[finding]` and reads both. The `code-review` summaries are
`rules[project-summary]` and `stack[project-summary]`.

A note with no downstream selector takes a bare name, as the `eval-annotation`
scanner's `open` does.

## Attachment names

An attachment name says what files it holds: `claude-config`,
`project-rule-files`, `project-stack-files`. A task that attaches declares the
names under `attaches`; a task that reads declares them under
`attachments.wants` or `attachments.needs`. A name is letters, digits, `-`,
`_`, and `.`.
