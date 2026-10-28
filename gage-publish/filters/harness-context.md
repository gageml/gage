Leave out what the harness placed in the model's context. The entries that
carry it are left out whole: the API request shape (the system prompt and
the tool definitions), the API request blobs (message content as sent, with
the reminders and instruction files in it), attachments (rendered reminders,
instruction files, skill and agent listings, hook output), and a user entry
the harness injected as meta. Entries are selected by type, so there is
nothing for a model to judge.
