+++
audience = ["public"]
description = "Removes personally identifiable information"

[[pattern]]
name = "email"
regex = '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'

[[pattern]]
name = "phone"
regex = '(?:^|[^0-9A-Za-z])((?:\+?[0-9]{1,3}[ .-])?\(?[0-9]{3}\)?[ .-][0-9]{3}[ .-][0-9]{4})(?:[^0-9A-Za-z]|$)'

[[pattern]]
name = "ssn"
regex = '(?:^|[^0-9-])([0-9]{3}-[0-9]{2}-[0-9]{4})(?:[^0-9-]|$)'

[[pattern]]
name = "git-user"
regex = '(?m)^Git user: (.+?)[ \t]*$'
+++

Remove anything whose disclosure would be an unwarranted invasion of personal
privacy: names of private individuals, usernames, email addresses, phone
numbers, postal addresses, and account and national identifiers, wherever they
appear in text. A public figure named in a public capacity, a project name, and
a company name are not personal data. Redact each value as a text redaction
with reason "pii", so every copy of it in the session is redacted.

{{ prompt }}
