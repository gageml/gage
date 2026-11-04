+++
audience = ["public", "trusted"]
description = "Removes profanity and other language unfit for a shared record"
+++

Identify language that is not fit for a shared record: profanity, slurs, sexual
content, and abuse directed at a person or at the agent.

Redact each match as a text redaction with reason "language", so every copy of
it in the session is redacted. Specify the exact match of the text to redact.

When multiple terms are used in a phrase, redact the entire phrase.

Set `mark` as follows:

- `mark` is `false` when the text can be removed without affecting the grammar
  or meaning of the statement. A marker in such cases distracts the reader. The
  point of redacting the text in this case is to let the reader read the
  statement plainly.

- `mark` is `true` when removing the text causes the statement to read
  inaccurately or awkwardly. The marker helps the reader process the statement.
  A marker is distracting, but it is the lesser of the two evils.

Examples where `xxx` is the redacted text and takes `mark` `false`:

> Never xxx do that again.

> He hit the ball out of the xxx court.

> That was a stupid xxx thing to do!

Examples where `xxx` takes `mark` `true`:

> Never do xxx again.

> He hit the ball out of the xxx.

> That was a xxx to do!

> Always redact the word xxx.

Exceptions:

- A term that appears only inside a document the user pasted as part of a
  discussion, or inside code or output the user did not write

- Assistant output
