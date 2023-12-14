# Run comments

The `run_comment` module provides an interface to run comments.

    >>> from gage._internal.run_comment import *
    >>> from gage._internal.types import *

Generate a run.

    >>> runs_dir = make_temp_dir()

    >>> from gage._internal.run_util import make_run

    >>> run = make_run(OpRef("test", "test"), runs_dir)

`get_comments()` returns a list of run comments. The initial list is
empty.

    >>> get_comments(run)
    []

Use `add_comment()` to add a comment to a run. The function logs a new
comment attribute and returns the new comment ID.

    >>> add_comment(run, "This is a comment.")  # +parse
    '{comment_id:comment_id}'

List the comments.

    >>> get_comments(run)  # +parse +json
    [
      [
        "{x:comment_id}",
        "{user}",
        {:timestamp_ms},
        "This is a comment."
      ]
    ]

    >>> assert x == comment_id

`user` is the system user.

    >>> from gage._internal.sys_config import get_user

    >>> assert user == get_user()

Add another comment.

    >>> add_comment(run, "Another comment here.")  # +parse
    '{comment2_id}'

List the comments.

    >>> get_comments(run)  # +parse +json
    [
      [
        "{x:comment_id}",
        "{}",
        {comment_ts:timestamp_ms},
        "This is a comment."
      ],
      [
        "{y:comment_id}",
        "{}",
        {comment2_ts:timestamp_ms},
        "Another comment here."
      ]
    ]

    >>> assert x == comment_id
    >>> assert y == comment2_id
    >>> assert comment_ts < comment2_ts

Use `set_comment()` to modify a comment message.

    >>> set_comment(run, comment_id, "I modified this.")

List the comments.

    >>> get_comments(run)  # +parse +json
    [
      [
        "{x:comment_id}",
        "{}",
        {x_ts:timestamp_ms},
        "Another comment here."
      ],
      [
        "{y:comment_id}",
        "{}",
        {y_ts:timestamp_ms},
        "I modified this."
      ]
    ]

Both the comment message and timestamp are updated.

    >>> assert y == comment_id
    >>> assert y_ts > comment_ts

The second comment now appears first in the list of sorted comments.

    >>> assert x == comment2_id
    >>> assert x_ts == comment2_ts

Use `delete_comment()` to delete a comment.

    >>> delete_comment(run, comment_id)

List the comments.

    >>> get_comments(run)  # +parse +json
    [
      [
        "{x:comment_id}",
        "{}",
        {:timestamp_ms},
        "Another comment here."
      ]
    ]

    >>> assert x == comment2_id
