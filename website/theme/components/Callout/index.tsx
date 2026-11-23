import type { CSSProperties, ReactNode } from "react";
import type { CalloutVariant } from "../../callout-variants";
import "./index.css";

export interface CalloutProps {
  type:
    | "tip"
    | "note"
    | "important"
    | "warning"
    | "caution"
    | "danger"
    | "info"
    | "details";
  title?: string;
  // Set by theme/remark-callout-variant.ts from a `:::details[variant]`
  // title prefix. Only meaningful on `type: 'details'`.
  variant?: CalloutVariant;
  children: ReactNode;
}

// Details callouts have no color of their own (--rp-container-details-*
// is always grey - see index.css). Retarget those custom properties at
// the chosen variant's palette instead of the container's grey ones, so
// every existing --rp-container-details-* consumer (border, background,
// title text, title marker) picks up the variant's colors unchanged.
function variantStyle(
  variant: CalloutProps["variant"],
): CSSProperties | undefined {
  if (!variant) return undefined;
  return {
    "--rp-container-details-border": `var(--rp-container-${variant}-border)`,
    "--rp-container-details-text": `var(--rp-container-${variant}-text)`,
    "--rp-container-details-bg": `var(--rp-container-${variant}-bg)`,
    "--rp-container-details-code-bg": `var(--rp-container-${variant}-code-bg)`,
  } as CSSProperties;
}

function capitalize(type: string) {
  return type.charAt(0).toUpperCase() + type.slice(1);
}

/**
 * Construct the DOM structure of the container directive.
 * For example:
 *
 * ::: tip {title="foo"}
 * This is a tip
 * :::
 *
 * will be transformed to:
 *
 * <div class="rp-callout rp-callout--tip">
 *   <div class="rp-callout__title">Tip</div>
 *   <div class="rp-callout__content">
 *     <p>This is a tip</p>
 *   </div>
 * </div>
 */
export function Callout({
  type,
  title,
  variant,
  children,
}: CalloutProps): ReactNode {
  const isDetails = type === "details";

  if (isDetails) {
    return (
      <details
        className={`rp-callout rp-callout--${type}`}
        style={variantStyle(variant)}
      >
        <summary className="rp-callout__title">
          {title ?? capitalize(type)}
        </summary>
        <div className="rp-callout__content">{children}</div>
      </details>
    );
  }

  return (
    <div className={`rp-callout rp-callout--${type}`}>
      <div className="rp-callout__title">{title ?? capitalize(type)}</div>
      <div className="rp-callout__content">{children}</div>
    </div>
  );
}

export default Callout;
