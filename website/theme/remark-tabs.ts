import type { Parent, Root, RootContent } from "mdast";
// Registers the directive node types (containerDirective et al.) in the
// mdast content maps via declaration merging.
import type {} from "mdast-util-directive";
import { visit } from "unist-util-visit";

// Resolve contiguous `:::tab[Label]{group=<id>}` directive blocks
// (parsed by remark-directive) to a runtime <Tabs groupId=<id>> element
// wrapping one <Tab label=Label> per block. Adjacent tab directives form
// one tab set; any other node ends the set. The group id is read from
// the first tab in a set and ignored on the rest. A tab with no label
// gets a positional default.

// biome-ignore lint/suspicious/noExplicitAny: mdx jsx mdast nodes
type MdxNode = any;

function isTabDirective(node: RootContent): boolean {
  return node.type === "containerDirective" && node.name === "tab";
}

function toTab(node: MdxNode, position: number): MdxNode {
  let label = "";
  const first = node.children[0];
  if (first?.data?.directiveLabel) {
    label = first.children
      .map((child: MdxNode) => child.value ?? "")
      .join("")
      .trim();
    node.children.shift();
  }
  return {
    type: "mdxJsxFlowElement",
    name: "Tab",
    attributes: [
      {
        type: "mdxJsxAttribute",
        name: "label",
        value: label || `Tab ${position + 1}`,
      },
    ],
    children: node.children,
  };
}

function toTabs(run: MdxNode[]): MdxNode {
  const group = run[0].attributes?.group;
  return {
    type: "mdxJsxFlowElement",
    name: "Tabs",
    attributes: group
      ? [{ type: "mdxJsxAttribute", name: "groupId", value: group }]
      : [],
    children: run.map(toTab),
  };
}

// `import { Tab, Tabs } from "@theme"` as an MDX ESM node, so plain .md
// files resolve the theme components without author-written imports.
function themeImportNode(): MdxNode {
  return {
    type: "mdxjsEsm",
    value: "",
    data: {
      estree: {
        type: "Program",
        sourceType: "module",
        body: [
          {
            type: "ImportDeclaration",
            specifiers: ["Tab", "Tabs"].map((name) => ({
              type: "ImportSpecifier",
              imported: { type: "Identifier", name },
              local: { type: "Identifier", name },
            })),
            source: { type: "Literal", value: "@theme", raw: '"@theme"' },
          },
        ],
      },
    },
  };
}

export function remarkTabs() {
  return (tree: Root) => {
    let used = false;
    visit(tree, (node) => {
      if (!("children" in node)) return;
      const children = (node as Parent).children;
      for (let i = 0; i < children.length; i++) {
        if (!isTabDirective(children[i])) continue;
        let end = i + 1;
        while (end < children.length && isTabDirective(children[end])) end++;
        children.splice(i, end - i, toTabs(children.slice(i, end)));
        used = true;
      }
    });
    if (used) {
      tree.children.unshift(themeImportNode());
    }
  };
}
