# Attr log

An attr log is a directory containing logged attributes. Attributes are
logged by adding new entries that are time-sortable. Previous attribute
values are not overwritten. Current values are derived by applying log
entries in time order.

    >>> from gage._internal.attr_log import *

## Basics

Create a directory for the attributes.

    >>> attrs_dir = make_temp_dir()

Attributes are read as a unit for a source directory. Attributes are
initially empty.

    >>> get_attrs(attrs_dir)
    {}

Attributes are logged in sets. Logged attributes must have an author. In
this case we use the author 'test'.

    >>> log_attrs(attrs_dir, "test", {"color": "green"})

Get the attrs.

    >>> get_attrs(attrs_dir)
    {'color': 'green'}

Attrs can be read by author.

    >>> get_attrs_by_author(attrs_dir)
    {'test': {'color': 'green'}}

    >>> get_attrs_by_author(attrs_dir, "test")
    {'test': {'color': 'green'}}

    >>> get_attrs_by_author(attrs_dir, "robert")
    {}

## Updating attributes

Attributes are updated by logging new values.

    >>> log_attrs(attrs_dir, "test", {"color": "blue"})

    >>> get_attrs(attrs_dir)
    {'color': 'blue'}

Attribute are delete by logging them as deleted.

    >>> log_attrs(attrs_dir, "test", {}, delete=["color"])

    >>> get_attrs(attrs_dir)
    {}

## Simulating distributed edits

The attribute log supports distributed attribute edits.

Consider two authors, Sam and Sarah.

Create attribute directories for each author.

    >>> sam_attrs = make_temp_dir()
    >>> sarah_attrs = make_temp_dir()

Sam assigns a label 'Good'.

    >>> log_attrs(sam_attrs, "Sam", {"label": "Good"})

    >>> get_attrs(sam_attrs)
    {'label': 'Good'}

Sarah doesn't yet see this label.

    >>> get_attrs(sarah_attrs)
    {}

`merge_attrs()` is a convenience function in `attr_log` for applying
attribute logs from one directory (src) to another directory (dest).

Apply Sam's logs to Sarah's.

    >>> merge_attrs(sam_attrs, sarah_attrs)

Sarah sees Sam's attributes.

    >>> get_attrs(sarah_attrs)
    {'label': 'Good'}

Sarah doesn't like the label and changes it.

    >>> log_attrs(sarah_attrs, "Sarah", {"label": "Bad"})

    >>> get_attrs(sarah_attrs)
    {'label': 'Bad'}

Sam still see's his label.

    >>> get_attrs(sam_attrs)
    {'label': 'Good'}

Both authors merge changes from one another. This is called
*synchronizing*.

    >>> merge_attrs(sam_attrs, sarah_attrs)
    >>> merge_attrs(sarah_attrs, sam_attrs)

Because Sarah's label is applied after Sam's, it appears in both sets of
attributes.

    >>> get_attrs(sam_attrs)
    {'label': 'Bad'}

    >>> get_attrs(sarah_attrs)
    {'label': 'Bad'}

However, each author can still see his or her own attributes as an
author.

    >>> get_attrs_by_author(sam_attrs)  # +pprint
    {'Sam': {'label': 'Good'}, 'Sarah': {'label': 'Bad'}}

    >>> get_attrs_by_author(sarah_attrs)  # +pprint
    {'Sam': {'label': 'Good'}, 'Sarah': {'label': 'Bad'}}

If Sam wants to see his view of the attributes, he can.

    >>> get_attrs_by_author(sam_attrs, "Sam")["Sam"]
    {'label': 'Good'}

Sarah can still see Sam's attributes.

    >>> get_attrs_by_author(sarah_attrs, "Sam")["Sam"]
    {'label': 'Good'}
