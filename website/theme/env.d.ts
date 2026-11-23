declare module "*.css";

interface ImportMetaEnv {
  readonly SSG_MD: boolean;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

// The topics the home page names, in order. Set by rspress.config.ts
// from topics.json.
declare const __GAGE_TOPICS__: {
  slug: string;
  title: string;
  example: boolean;
}[];
