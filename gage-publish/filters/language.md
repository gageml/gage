Remove language that is not fit for a public record: profanity, slurs,
sexual content, and abuse directed at a person or at the agent. Judge each
occurrence in its context. An expletive in a user's outburst is removed; the
same word quoted inside a document the user pasted as part of a discussion,
or inside code or output the user did not write, is left in place.
Assistant output stays verbatim. Redact as a span redaction with reason
"language": mode "remove" when the sentence reads the same without the span,
mode "mark" when its absence would be noticed. Candid material that is not
profanity, such as venting about a colleague, takes reason "privacy".
