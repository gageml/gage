---
status: proposed
---

# Object type version changes

An object's type is named in the `type` file of its tree.

Example:

```
gage::session 1
```

The first part is the type name and the second part is the type version.

A release of Gage MUST read, edit, and merge objects of every type version that
an earlier release wrote.

A release of Gage MUST refuse to read, edit, or merge an object whose type
version is higher than the release supports. An earlier release therefore
cannot use an object of a new type version, and such objects reach stores that
run earlier releases through push, fetch, and merge.

In practice, type versions should not change. A change to an object's layout
MUST use these methods where they suffice:

- Treat a missing file or value with a correct default. A default is correct
  when every feature, new and existing, behaves correctly with it.

- Ignore unrecognized files and values.

- Read an unrecognized enumeration value as unknown.

- Keep unrecognized files and values when editing an object.

- Add a file or value for newly required data. An existing file or value keeps
  its meaning and is not removed.

The type version MUST be changed when these methods do not suffice:

- No correct default exists for a missing file or value.

- An earlier release that ignores the change would behave incorrectly.

An edit keeps the object's type version unless the edit requires the layout of
a higher version.
