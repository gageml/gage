import type { Root } from "mdast";
// Registers the directive node types (containerDirective et al.) in the
// mdast content maps via declaration merging.
import type {} from "mdast-util-directive";
import { visit } from "unist-util-visit";

// Resolve a `:::pullquote` directive block (parsed by remark-directive)
// to an <aside class="pullquote"> element. The block's children are
// kept as-is; theme/index.css styles the element with a large open
// quote mark and no attribution.

// biome-ignore lint/suspicious/noExplicitAny: mdx jsx mdast nodes
type MdxNode = any;

export function remarkPullquote() {
  return (tree: Root) => {
    visit(tree, "containerDirective", (node, index, parent) => {
      if (index === undefined || !parent || node.name !== "pullquote") return;
      (parent.children as MdxNode[])[index] = {
        type: "mdxJsxFlowElement",
        name: "aside",
        attributes: [
          { type: "mdxJsxAttribute", name: "className", value: "pullquote" },
        ],
        children: node.children,
      };
    });
  };
}
