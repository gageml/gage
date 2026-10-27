Identify language that is not fit for a public record: profanity, slurs, sexual
content, and abuse directed at a person or at the agent.

Specify the exact match of the text to redact.

When multiple terms are used in a phrase, redact the entire phrase.

Determine the `no_mark_flag` as follows:

- If the text can be removed without affecting the grammar or meaning of a
  statement, do not include a mark. Including a mark in such cases distracts the
  user. The point of redacting the text in this case is let the user read the
  statement plainly.

- If removing the text causes the statement to read inaccurately or awkwardly,
  include a mark to help the reader process the statement. While a mark is
  distracting, it is the lesser of the two evils.

Examples where `xxx` is the redacted text and should NOT include a marker:

> Never xxx do that again.

> He hit the ball out of the xxx court.

> That was a stupid xxx thing to do!

Examples where `xxx` SHOULD include a marker:

> Never do xxx again.

> He hit the ball out of the xxx.

> That was a xxx to do!

> Always redact the word xxx.

Exceptions:

- A term that appears only inside a document the user pasted as part of a
  discussion, or inside code or output the user did not write

- Assistant output
