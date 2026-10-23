Remove every credential or key whose leak enables unauthorized access or
impersonation: passwords, API keys, access tokens, bearer tokens, private
keys, signing secrets, and connection strings that embed a password. A
public identifier such as a username, a client id, or a key's name is not a
secret. Redact each value as a text redaction with reason "secret", so every
copy of it in the session is redacted, including copies inside tool inputs
and tool results.
