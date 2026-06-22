Counts of assistant messages in a session, keyed by model id.

**value** — JSON object mapping the model id reported on each assistant
message (`message.model` in the raw entry) to the number of assistant
messages produced by that model. Assistant messages with no recorded
model id are recorded under the key `"<unknown>"`. An empty object
means the session contains no assistant messages.

**target** — `{ session }`.

The note surfaces mid-session model switches and the relative share of
each model used. Multiple keys imply the session ran across more than
one model — common for `/model` switches or harness fallbacks. The
note does not record which turns used which model or why a switch
occurred.
