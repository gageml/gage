Number of API error entries the harness recorded in a session.

**value** — integer count of entries the harness writes to record an
API call failure (typically user-type entries flagged with
`isApiErrorMessage: true`). Zero is written.

**target** — `{ session }`.

API errors include rate limits, overload, network failures, and
provider-side errors. A nonzero count means the harness surfaced at
least one such failure inline; it does not record the error class,
whether the call eventually succeeded on retry, or whether the user
intervened.
