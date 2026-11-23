import type { Root } from "mdast";
import { visit } from "unist-util-visit";

// Resolve `![alt](icon:lucide/<name>)` image syntax to a runtime <Icon>
// component. The name is passed through untouched; resolution and
// fault-tolerance for unknown names happen in theme/Icon.tsx.
const PREFIX = "icon:lucide/";

export function remarkLucide() {
  return (tree: Root) => {
    visit(tree, "image", (node, index, parent) => {
      if (index === undefined || !parent || !node.url.startsWith(PREFIX))
        return;

      const attributes: unknown[] = [
        {
          type: "mdxJsxAttribute",
          name: "name",
          value: node.url.slice(PREFIX.length),
        },
      ];
      if (node.alt) {
        attributes.push({
          type: "mdxJsxAttribute",
          name: "label",
          value: node.alt,
        });
      }

      // biome-ignore lint/suspicious/noExplicitAny: mdx jsx mdast nodes
      (parent.children as any[])[index] = {
        type: "mdxJsxTextElement",
        name: "Icon",
        attributes,
        children: [],
      };
    });
  };
}
