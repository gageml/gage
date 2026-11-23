// Variant names a `:::details[variant]` callout may carry. Single source
// of truth for theme/remark-callout-variant.ts (which parses the prefix)
// and theme/components/Callout (which types the prop).
export const CALLOUT_VARIANTS = [
  "tip",
  "note",
  "important",
  "warning",
  "caution",
  "danger",
  "info",
] as const;

export type CalloutVariant = (typeof CALLOUT_VARIANTS)[number];
