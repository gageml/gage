Remove information that describes the machine or network the session ran on
and that would help someone target it: absolute filesystem paths, including
the working directory and the home directory, hostnames, IP addresses,
internal URLs, the remotes of version control hosts, and operating system
and kernel versions. A relative path inside the project, a public package
name, and a public website named in discussion are not system information.
Redact each value as a text redaction with reason "system", so every copy of
it in the session is redacted, including copies inside tool inputs and tool
results.
