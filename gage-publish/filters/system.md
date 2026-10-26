The fields whose type says they hold a path, a host, or an OS version are
left out of the published session: the working directory, the home
directory, the git state, the trusted directories, the rule roots, the
remotes, and the environment snapshot. Remove what remains in text and
would help someone target the machine or network the session ran on: an
absolute filesystem path, a hostname, an IP address, an internal URL, the
remote of a version control host, and an operating system or kernel version
quoted in a prompt, a tool call, a tool result, or a reply. A relative path
inside the project, a public package name, and a public website named in
discussion are not system information. Redact each value as a text
redaction with reason "system", so every copy of it in the session is
redacted, including copies inside tool inputs and tool results.
