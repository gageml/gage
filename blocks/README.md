# Blocks

A block is a short human owned and reviewed (AI assist is fine) claim of fact.

Blocks serve as long running anchors for model state.

It's important that blocks are human owned and carefully reviewed. Blocks must
not be authored by models without careful, reasoned review by humans.

Blocks serve as a guard against noise amplification in model context.

Blocks are high signal and stable.

Blocks must be updated to align with current understanding.

Blocks are organized in a hierarchy. Each directory contains a type of block as
described by the README.md

Blocks may define an optional `status` value of one of:

- `draft`
- `proposed`
- `rejected`
- `abandoned`
- `superseded`
- `active`

If a `status` value is not defined, it is assumed to be `draft`.

If a block is superseded, it's new block must be named using a `superseded-by`
attribute.
