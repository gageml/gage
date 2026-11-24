// Shapes the output that `llms: true` generates for language models:
// the llms.txt index and the Markdown copy of each page.

export interface SidebarItem {
  text: string;
  link?: string;
  collapsed?: boolean;
  items?: SidebarItem[];
}

interface LlmsPage {
  routePath: string;
  link: string;
  title: string;
  description?: string;
}

interface LlmsContext {
  title?: string;
  description?: string;
  sections: { title: string; pages: LlmsPage[] }[];
}

function routeKey(route: string): string {
  const key = route
    .replace(/\.html$/, "")
    .replace(/\/index$/, "/")
    .replace(/\/$/, "");
  return key || "/";
}

function sidebarLinks(items: SidebarItem[]): string[] {
  return items.flatMap((item) => [
    ...(item.link ? [item.link] : []),
    ...sidebarLinks(item.items ?? []),
  ]);
}

// Rspress groups llms.txt by the top nav, which has two entries, so
// nearly every page lands in its "Others" section. Group by the
// top-level sidebar sections instead, in sidebar order. A page that the
// sidebar does not list goes in a final "Other" section.
export function llmsTxtBySidebar(sidebar: SidebarItem[]) {
  return ({ title, description, sections }: LlmsContext): string => {
    const pages = new Map<string, LlmsPage>();
    for (const section of sections) {
      for (const page of section.pages) {
        pages.set(routeKey(page.routePath), page);
      }
    }

    const groups: { title: string; pages: LlmsPage[] }[] = [];
    for (const group of sidebar) {
      const found: LlmsPage[] = [];
      for (const link of sidebarLinks([group])) {
        const page = pages.get(routeKey(link));
        if (!page) continue;
        found.push(page);
        pages.delete(routeKey(link));
      }
      if (found.length > 0) groups.push({ title: group.text, pages: found });
    }
    if (pages.size > 0) {
      groups.push({ title: "Other", pages: [...pages.values()] });
    }

    const lines = [`# ${title ?? ""}`.trimEnd()];
    if (description) lines.push("", `> ${description}`);
    for (const group of groups) {
      lines.push("", `## ${group.title}`, "");
      for (const page of group.pages) {
        const suffix = page.description ? `: ${page.description}` : "";
        lines.push(`- [${page.title}](${page.link})${suffix}`);
      }
    }
    return `${lines.join("\n")}\n`;
  };
}

// GitHub alert kinds for the Rspress callout types.
const ALERTS: Record<string, string> = {
  tip: "TIP",
  note: "NOTE",
  info: "NOTE",
  important: "IMPORTANT",
  warning: "WARNING",
  caution: "CAUTION",
  danger: "CAUTION",
};

// `:::name[label]{attrs} title`, as the Markdown build writes it. That
// build does not parse directives, so it backslash-escapes the label
// and attribute brackets.
const OPEN = /^:::([A-Za-z][\w-]*)(?:\\?\[(.*?)\])?(?:\\?\{.*?\})?[ \t]*(.*)$/;
const FENCE = /^ {0,3}(`{3,}|~{3,})/;

interface Block {
  name: string;
  label: string;
  title: string;
  lines: string[];
}

function trimBlank(lines: string[]): string[] {
  let start = 0;
  let end = lines.length;
  while (start < end && lines[start].trim() === "") start++;
  while (end > start && lines[end - 1].trim() === "") end--;
  return lines.slice(start, end);
}

function quote(lines: string[]): string[] {
  return lines.map((line) => (line === "" ? ">" : `> ${line}`));
}

function renderBlock({ name, label, title, lines }: Block): string[] {
  const body = trimBlank(lines);
  if (name === "tab") return [`**${label || "Tab"}**`, "", ...body];
  if (name === "pullquote") return quote(body);
  const heading = title ? [`**${title}**`, ""] : [];
  if (name === "details") return quote([...heading, ...body]);
  return quote([`[!${ALERTS[name]}]`, ...heading, ...body]);
}

// The Markdown build runs none of this site's remark plugins, so the
// `:::` blocks that those plugins and Rspress resolve for HTML reach
// the Markdown copies as raw directive lines. Rewrite each block as
// plain Markdown: a callout becomes an alert blockquote and a tab
// becomes its label in bold above its content. Fenced code is left as
// written.
export function plainDirectives(markdown: string): string {
  const out: string[] = [];
  const stack: Block[] = [];
  let fence: string | null = null;

  const emit = (lines: string[]) => {
    const top = stack[stack.length - 1];
    (top ? top.lines : out).push(...lines);
  };

  for (const line of markdown.split("\n")) {
    const fenceMatch = line.match(FENCE);
    if (fence) {
      if (fenceMatch?.[1].startsWith(fence) && line.trim() === fenceMatch[1]) {
        fence = null;
      }
      emit([line]);
      continue;
    }
    if (fenceMatch) {
      fence = fenceMatch[1];
      emit([line]);
      continue;
    }

    if (line.trim() === ":::" && stack.length > 0) {
      const block = stack.pop() as Block;
      emit(renderBlock(block));
      continue;
    }
    const open = line.match(OPEN);
    const name = open?.[1] ?? "";
    if (open && (name in ALERTS || /^(tab|pullquote|details)$/.test(name))) {
      stack.push({ name, label: open[2] ?? "", title: open[3], lines: [] });
      continue;
    }
    emit([line]);
  }

  // An unclosed block still renders, so no content is dropped.
  while (stack.length > 0) {
    const block = stack.pop() as Block;
    emit(renderBlock(block));
  }
  return out.join("\n");
}
