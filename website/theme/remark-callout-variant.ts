import type { Root } from "mdast";
import { visit } from "unist-util-visit";
import { CALLOUT_VARIANTS } from "./callout-variants";

// Rspress's own container-syntax transform (bundled in @rspress/core,
// runs before any project remarkPlugins - see rspress.config.ts) turns
// `:::details ...` into a containerDirective node named CALLOUT_NAME,
// with the entire rest of the line as its title. This plugin runs after
// that and looks for an optional `[variant]` prefix on a details
// callout's title, e.g. `:::details[warning] Some title`, and moves it
// to a `variant` prop so theme/components/Callout can add a second
// `rp-callout--{variant}` styling hook without Rspress's own directive
// syntax needing to know about it.

// Name @rspress/core's containerSyntax.js assigns to every `:::type`
// callout it recognizes (tip/note/.../details); tied to that internal
// constant, so re-verify against @rspress/core's containerSyntax.js
// (RSPRESS_FORK_VERSION in rspress.config.ts) on every upstream bump.
const CALLOUT_NAME = "$$$callout$$$";

const VARIANT_PREFIX = new RegExp(`^\\[(${CALLOUT_VARIANTS.join("|")})\\]\\s*`);

// biome-ignore lint/suspicious/noExplicitAny: mdast-util-directive node, extended by Rspress with attributes/data.hProperties it doesn't type
type CalloutNode = any;

export function remarkCalloutVariant() {
  return (tree: Root) => {
    visit(tree, "containerDirective", (node: CalloutNode) => {
      if (node.name !== CALLOUT_NAME || node.attributes?.type !== "details") {
        return;
      }
      const title: string | undefined = node.attributes?.title;
      const match = title?.match(VARIANT_PREFIX);
      if (!match) return;
      const [prefix, variant] = match;
      const strippedTitle = title.slice(prefix.length);
      node.attributes.title = strippedTitle;
      node.attributes.variant = variant;
      if (node.data?.hProperties) {
        node.data.hProperties.title = strippedTitle;
        node.data.hProperties.variant = variant;
      }
    });
  };
}
