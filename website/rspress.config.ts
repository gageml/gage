import * as fs from "node:fs";
import * as path from "node:path";
import type { RspressPlugin } from "@rspress/core";
import { defineConfig } from "@rspress/core";
import { pluginSitemap } from "@rspress/plugin-sitemap";
import remarkDirective from "remark-directive";
import remarkSmartypants from "remark-smartypants";
import { cliLang } from "./theme/cli-lang";
import {
  llmsTxtBySidebar,
  plainDirectives,
  type SidebarItem,
} from "./theme/llms";
import { remarkCalloutVariant } from "./theme/remark-callout-variant";
import { remarkLucide } from "./theme/remark-lucide";
import { remarkPullquote } from "./theme/remark-pullquote";
import { remarkTabs } from "./theme/remark-tabs";

// theme/index.tsx shadows Outline and Callout (an ejected component
// under theme/components/Callout) and imports dist CSS by path; all of
// this assumes this exact upstream version. Re-diff the fork before
// bumping, including re-running `rspress eject Callout` and re-applying
// the variant prop in theme/components/Callout/index.tsx. The eject
// writes index.scss, which this build cannot compile (no Sass plugin):
// delete it and copy node_modules/@rspress/core/dist/theme/components/
// Callout/index.css in its place, keeping the `./index.css` import.
const RSPRESS_FORK_VERSION = "2.0.20";
const installed = (
  JSON.parse(
    fs.readFileSync(
      path.join(__dirname, "node_modules/@rspress/core/package.json"),
      "utf8",
    ),
  ) as { version: string }
).version;
if (installed !== RSPRESS_FORK_VERSION) {
  throw new Error(
    `@rspress/core ${installed} != ${RSPRESS_FORK_VERSION}: theme/index.tsx forks Outline and imports dist CSS paths. Re-verify both against the new version, then update RSPRESS_FORK_VERSION in rspress.config.ts.`,
  );
}

// The docs are the Markdown files under ../docs, kept alongside the
// source they document. This site renders them and adds a landing page.
const DOCS = path.resolve(__dirname, "../docs");

// Mount every .md/.mdx under the docs dir at the site root, using its
// path relative to that dir as the route.
function gageDocsPlugin(docsDir: string): RspressPlugin {
  return {
    name: "gage-docs",
    addPages() {
      if (!fs.existsSync(docsDir)) return [];
      const files: string[] = [];
      const walk = (dir: string) => {
        for (const entry of fs.readdirSync(dir, {
          withFileTypes: true,
        })) {
          const full = path.join(dir, entry.name);
          if (entry.isDirectory()) walk(full);
          else if (/\.mdx?$/.test(entry.name)) files.push(full);
        }
      };
      walk(docsDir);
      return files.map((filepath) => {
        const relPath = path.relative(docsDir, filepath).replace(/\.mdx?$/, "");
        return { routePath: `/${relPath}`, filepath };
      });
    },
  };
}

// Topics are the things a person can learn from their logs. The home
// page names them in its marquee and draws its examples from them. They
// are listed in topics.json. A topic has an example when a file named
// for its slug is under pages/public/examples.
interface Topic {
  slug: string;
  title: string;
  example: boolean;
}

const TOPICS: Topic[] = (
  JSON.parse(fs.readFileSync(path.join(__dirname, "topics.json"), "utf8")) as {
    slug: string;
    title: string;
  }[]
).map(({ slug, title }) => ({
  slug,
  title,
  example: fs.existsSync(
    path.join(__dirname, "pages/public/examples", `${slug}.json`),
  ),
}));

const SIDEBAR: SidebarItem[] = [
  {
    text: "Introduction",
    items: [
      { text: "Quick start", link: "/start" },
      { text: "Uninstall", link: "/uninstall" },
    ],
  },
  {
    text: "Concepts",
    collapsed: true,
    items: [
      { text: "Sessions", link: "/sessions" },
      { text: "Scans", link: "/scans" },
      { text: "Scanners", link: "/scanners" },
      { text: "Issues", link: "/issues" },
      { text: "Notes", link: "/notes" },
    ],
  },
  {
    text: "How to",
    collapsed: false,
    items: [
      {
        text: "Session history",
        items: [
          {
            text: "See context for a code change",
            link: "/how-to/blame",
          },
          { text: "Recall a decision", link: "/how-to/recall" },
        ],
      },
      {
        text: "Measurement",
        items: [{ text: "Scan a session", link: "/how-to/scan" }],
      },
      {
        text: "Workflow",
        items: [
          {
            text: "Take up work from another session",
            link: "/how-to/take-up-work",
          },
        ],
      },
    ],
  },
  {
    text: "Commands",
    collapsed: true,
    items: [
      { text: "init", link: "/commands/init" },
      { text: "status", link: "/commands/status" },
      { text: "scan", link: "/commands/scan" },
      { text: "resolve", link: "/commands/resolve" },
      { text: "session", link: "/commands/session" },
      { text: "dataset", link: "/commands/dataset" },
      { text: "issue", link: "/commands/issue" },
      { text: "note", link: "/commands/note" },
      { text: "tag", link: "/commands/tag" },
      { text: "attachment", link: "/commands/attachment" },
      { text: "query", link: "/commands/query" },
      { text: "blame", link: "/commands/blame" },
      { text: "config", link: "/commands/config" },
      { text: "native", link: "/commands/native" },
      { text: "remote", link: "/commands/remote" },
      { text: "push", link: "/commands/push" },
      { text: "pull", link: "/commands/pull" },
      { text: "fetch", link: "/commands/fetch" },
      { text: "merge", link: "/commands/merge" },
      { text: "backup", link: "/commands/backup" },
      { text: "store", link: "/commands/store" },
      { text: "index", link: "/commands/index" },
      { text: "mcp", link: "/commands/mcp" },
    ],
  },
  {
    text: "Scanners",
    collapsed: true,
    items: [
      { text: "checks", link: "/scanners/checks" },
      { text: "code-review", link: "/scanners/code-review" },
      {
        text: "eval-annotation",
        link: "/scanners/eval-annotation",
      },
      { text: "findings", link: "/scanners/findings" },
      { text: "general", link: "/scanners/general" },
      {
        text: "session-retention",
        link: "/scanners/session-retention",
      },
    ],
  },
  {
    text: "Scanner development",
    collapsed: true,
    items: [
      { text: "Naming conventions", link: "/scanner-dev/names" },
      { text: "Language server", link: "/scanner-dev/lsp" },
    ],
  },
  {
    text: "Reference",
    collapsed: true,
    items: [{ text: "Tools", link: "/tools" }],
  },
];

const OUT_DIR = path.join(__dirname, "doc_build");

// Rewrite the Markdown copies and llms-full.txt once the build has
// written them. Rspress has no hook into its Markdown rendering.
function gageLlmsMarkdownPlugin(): RspressPlugin {
  return {
    name: "gage-llms-markdown",
    afterBuild(_config, isProd) {
      if (!isProd) return;
      const rewrite = (file: string) => {
        const before = fs.readFileSync(file, "utf8");
        const after = plainDirectives(before);
        if (after !== before) fs.writeFileSync(file, after);
      };
      const walk = (dir: string) => {
        for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
          const full = path.join(dir, entry.name);
          if (entry.isDirectory()) walk(full);
          else if (entry.name.endsWith(".md")) rewrite(full);
        }
      };
      walk(OUT_DIR);
      rewrite(path.join(OUT_DIR, "llms-full.txt"));
    },
  };
}

export default defineConfig({
  root: path.join(__dirname, "pages"),
  plugins: [gageDocsPlugin(DOCS), pluginSitemap(), gageLlmsMarkdownPlugin()],
  outDir: OUT_DIR,
  title: "Gage",
  siteOrigin: "https://docs.gage.io",
  llms: { llmsTxt: llmsTxtBySidebar(SIDEBAR) },
  icon: "/favicon.ico",
  builderConfig: {
    source: {
      define: {
        __GAGE_TOPICS__: JSON.stringify(TOPICS),
      },
    },
    html: {
      tags: [
        {
          tag: "link",
          attrs: { rel: "preconnect", href: "https://fonts.googleapis.com" },
        },
        {
          tag: "link",
          attrs: {
            rel: "preconnect",
            href: "https://fonts.gstatic.com",
            crossorigin: "",
          },
        },
        {
          tag: "link",
          attrs: {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Outfit:wght@100..900&display=swap",
          },
        },
      ],
    },
  },
  markdown: {
    globalComponents: [path.join(__dirname, "theme/Icon.tsx")],
    remarkPlugins: [
      remarkDirective,
      remarkTabs,
      remarkLucide,
      remarkPullquote,
      remarkCalloutVariant,
      [remarkSmartypants, { dashes: "oldschool" }],
    ],
    shiki: {
      langs: [cliLang],
    },
  },
  head: [
    [
      "link",
      {
        rel: "icon",
        type: "image/png",
        sizes: "32x32",
        href: "/favicon-32x32.png",
      },
    ],
    [
      "link",
      {
        rel: "icon",
        type: "image/png",
        sizes: "16x16",
        href: "/favicon-16x16.png",
      },
    ],
    [
      "link",
      {
        rel: "apple-touch-icon",
        sizes: "180x180",
        href: "/apple-touch-icon.png",
      },
    ],
    ["link", { rel: "manifest", href: "/site.webmanifest" }],
  ],
  route: {
    cleanUrls: true,
  },
  i18nSource: {
    nextPageText: { en: "Next" },
    prevPageText: { en: "Prev" },
  },
  themeConfig: {
    socialLinks: [
      {
        icon: "github",
        mode: "link",
        content: "https://github.com/gageml/gage",
      },
      {
        icon: "discord",
        mode: "link",
        content: "/discord",
      },
    ],
    nav: [
      { text: "Docs", link: "/docs/", activeMatch: "/docs/" },
      { text: "Scanners", link: "/scanners", activeMatch: "/scanners" },
    ],
    sidebar: { "/": SIDEBAR },
  },
});
