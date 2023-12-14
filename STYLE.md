# Style Guidelines

As specified in `CONTRIBUTING.md`, patches to this project SHOULD adhere
to the styles specified in this document.

The key words “MUST”, “MUST NOT”, “REQUIRED”, “SHALL”, “SHALL NOT”,
“SHOULD”, “SHOULD NOT”, “RECOMMENDED”, “MAY”, and “OPTIONAL” in this
document are to be interpreted as described in [RFC
2119](https://datatracker.ietf.org/doc/html/rfc2119).

The terms "PATCH", "CONTRIBUTOR", and "MAINTAINER" used in this
document are defined in `CONTRIBUTING.md`.

## Python source code formatting

### Rules

1. Code SHOULD clearly reflect the intention of its author. Code that
   is confusing to readers MAY be rewritten by any Contributor with
   the intent of improving its clarity.

2. A section of code SHOULD be understandable upon reading in a short
   period of time (e.g. less than 2-3 minutes).

3. Functions SHOULD be used in favor of code block to define the scope
   of code.

4. Object oriented patterns (classes and inheritance) SHOULD be avoided
   in favor of functions.

5. Object oriented patterns MAY be used when there is no better
   alternative.

6. All variable and function names MUST use [snake
   case](https://en.wikipedia.org/wiki/Snake_case).

7. All class names MUST use [upper camel
   case](https://en.wikipedia.org/wiki/Camel_case).

8. Functions intended for use within a module are considered "private"
   and MUST be named with a leading underscore character.

9. Functions used outside a module are considered "public" and MUST
   NOT be named with a leading underscore.

10. When using classes, avoid static methods (i.e. methods defined
    using the `staticmethod` decorator) and instead use module level
    private functions that are associated with the class in proximity
    within the module source code file.

11. Names SHOULD be as short as possible without compromising clarity.

12. Names SHOULD follow established naming conventions and patterns in
    the project code base. Contributors SHALL be free to rename
    variables, functions, etc. to improve readability or improve the
    consistency of code. Such changes MUST NOT be capricious or
    arbitrary and MAY be reverted if deemed so by maintainers.

### Automatic formatting

This project uses Black to format Python source code.

To show proposed changes by Black to the project, run:

    $ black . --preview

To apply changes, run the command without `--preview`.

Options for Black `pyproject.toml` in the `[tool.black]` section.

### Quotations

Strings MUST be quoted using double-quotes (`"`) even in cases where the
string contains double quotes. In that case, string double quotes must
be escaped, e.g. `"Hello \"World\""`.

This convention simplifies search/replace of strings throughout the
project. Black must be configured with `skip-string-normalization` to
prevent it from reformatting strings en mass throughout the project
code.

### Other style considerations

Where Black formatting does not apply, consult [PEP
8](https://peps.python.org/pep-0008/).
