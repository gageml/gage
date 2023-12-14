# `comment` command

    >>> run("gage comment -h")
    Usage: gage comment [options] [run]...
    ⤶
      Comment on runs.
    ⤶
      Uses the system editor by default to add a comment to
      one or more runs. Use '-m / --message' to specify the
      comment message as a command argument rather than use
      the system editor.
    ⤶
      Use '--list' to list run comments.
    ⤶
      Use '--delete' with a comment ID to delete a comment.
    ⤶
      Use '--edit' to edit a comment.
    ⤶
    Arguments:
      [run]...  Runs to modify. Required unless '--all' is
                specified.
    ⤶
    Options:
      -m, --msg msg         Message used for the comment. If
                            not specified for a comment, the
                            system editor opened.
      -d, --delete comment  Delete comment. Use '--list' to
                            show comments.
      -e, --edit comment    Edit comment. Use '--list' show
                            show comments.
      -l, --list            Show run comments.
      -w, --where expr      Modify runs matching filter
                            expression.
      -a, --all             Modify all runs.
      -y, --yes             Make changes without prompting.
      -h, --help            Show this message and exit.
    <0>

Generate a run to comment on.

    >>> use_example("hello")

    >>> run("gage run -qy")
    <0>

Show comments.

    >>> run("gage comment --list")
    <0>

Add a comment.

    >>> run("gage comment -m 'Hello with defaults' 1")  # +parse
    Added comment to run {run_name:run_name}
    <0>

Show comments.

    >>> run("gage comment --list")  # +parse -space
    {comment_id:comment_id} {x:run_name}
    | author      {}                                           |
    | date        {:datetime}                                  |
    | run         {y:run_name}                                 |
    | comment id  {z:comment_id}                               |
    |                                                          |
    | Hello with defaults                                      |
    <0>

    >>> assert z == comment_id
    >>> assert x == y == run_name

Comments appear in `show`.

    >>> run("gage show")  # +parse -space
    {:run_id}
    | hello:hello                                    completed |
    ⤶
    {}
                              Comments
    | {}                                                    {} |
    | -------------------------------------------------------- |
    | Hello with defaults                                      |
    <0>

Modify the comment.

    >>> run(f"gage comment --edit {comment_id} -m 'Modified comment' -y")
    ... # +parse
    Modified comment {x:comment_id}
    <0>

    >>> assert x == comment_id

    >>> run("gage comment --list")  # +parse -space
    {x:comment_id} {y:run_name}
    | author      {}                                           |
    | date        {:datetime}                                  |
    | run         {:run_name}                                  |
    | comment id  {:comment_id}                                |
    |                                                          |
    | Modified comment                                         |
    <0>

    >>> assert x == comment_id
    >>> assert y == run_name

Delete the comment.

    >>> run(f"gage comment --delete {comment_id} {run_name} -y")
    ... # +parse
    Delete comment {x:comment_id} from run {y:run_name}
    <0>

    >>> assert x == comment_id
    >>> assert y == run_name

    >>> run("gage comment --list")
    <0>

## Errors

When adding a comment, a run specifier is required.

    >>> run("gage comment -m 'A thought'")
    Specify a run to modify or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage comment -h' for additional help.
    <1>
