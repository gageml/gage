API error

API errors include rate limits, overload, network failures, and provider-side
errors. A nonzero count means the harness surfaced at least one such failure
inline; it does not record the error class, whether the call eventually
succeeded on retry, or whether the user intervened.

**value** - error message

**target** - session line

**metadata** - `error` record detail
