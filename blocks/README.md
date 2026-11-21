# Blocks

A block is a core principle or decision that guides future decisions. A person
or a model reads a block to decide how to design or change something.

Blocks serve as long running anchors for human understanding and model context.

The purpose of blocks is to mitigate noise in context. Blocks must therefore be
carefully curated by human experts to say precisely and accurately things that
should shape future decisions and changes.

## What a block is not

A block is not a rule. A rule is an instruction to do or not do a specific
thing. Rules belong in `CLAUDE.md`.

A block does not describe how the code implements a decision. It does not name
functions, files, or data layouts unless the decision is about them.

## Blocks and source code

A block records what was decided. Source code records what Gage does. When the
two disagree, one of them is wrong, and a human decides which one changes.

## What a block must be

- Human owned. A model may assist, but a block must not be authored by a model
  without careful, reasoned review by a human.

- Deliberated. A block reflects careful deliberation by humans in all cases.

- Short. A block states one principle.

- Stable. A block holds when the code is rewritten. A block is updated when the
  understanding behind it changes.

## Organization

Blocks are organized in a hierarchy. Each directory contains a type of block as
described by the README.md

## Status

Blocks may define an optional `status` value of one of:

- `draft`
- `proposed`
- `rejected`
- `abandoned`
- `superseded`
- `active`

If a `status` value is not defined, it is assumed to be `draft`.

If a block is superseded, its replacement must be named using a `superseded-by`
attribute.
