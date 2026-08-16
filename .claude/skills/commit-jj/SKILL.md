Review the pending changes using `jj` and present a series of proposed
`jj commit` commands for each distinct, non-overlapping commits.

Use the 50/72 rule.

Write subject lines in telegraphic style: omit articles ("a", "an", "the") when
the line stays readable without them. Example: "Render markdown tables in TUI",
not "Render markdown tables in the TUI". Keep articles only when dropping one
creates ambiguity.

Only include message bodies if they add essential context to the title.

Do not run the commands. Only present them for copying.

When presenting the command, specify the files before the commit message.
Example: `jj commit foo.txt bar.txt -m "Sample message"`

If each change breaks cleanly into its own commit, present only the commands.
Oterwise call attention to the fact a file includes multiple commits.

Present each command its own fenced block.
