## Biome

- Run `just check` before declaring any change complete. It runs the full gate
  (Biome lint + format, with auto-fix) as one command; partial verification is
  not an option.

## Rspress

This site uses Rspress.

To answer questions about Rspress, use a local copy of the Rspress repo under
.vendor-cache.

```shell
git clone --depth 1 git@github.com:web-infra-dev/rspress.git .vendor-cache/rspress
```

From there you can find and search files efficiently.

## User documentation

This applies to all user-facing content.

Always:

- Use Hemingway style if Hemingway wrote about technical matters. Be short and
  to the point.

- Use full, grammatically correct sentences instead of fragments separated by
  em-dashes, semi-colons, or other non-grammatical tactics

- Use continuing phrases like "such as", "for example" or "including" in a full
  complete sentence when you want to provide additional details. If the sentence
  is too long --- you are using Hemingway style --- consider using a separate
  sentence with proper grammar to provide the added detail or context. For
  example, rather than "Annotates sessions: brief, unstructured observations."
  use "The scanner annotates sessions with brief, unstructured observations."
  Or, "The scanner annotates sessions. Annotations include brief, unstructured
  observations."
