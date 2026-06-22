Span of a session in milliseconds

**value** - integer: the timestamp of the last message-row entry minus
the timestamp of the first, in milliseconds. Zero is written for
sessions with fewer than two timestamped message rows.

**target** - session

The span is wall-clock between the first and last recorded message
entry. It includes idle time between the user's turns and is not a
measure of model compute time. A long span with few messages indicates
an intermittent session; a short span with many messages indicates
sustained back-and-forth.
