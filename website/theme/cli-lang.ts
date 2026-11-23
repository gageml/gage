// Shell code fences mis-highlight CLI synopsis text: the bash grammar
// tokenizes `<` and `>` in placeholders like <COMMAND> as redirection
// operators, splitting the word across token boundaries. This grammar
// highlights command invocations without shell semantics. Scope names
// must come from the fixed set mapped by Shiki's css-variables theme
// (token-keyword, token-parameter, token-function, ...); other scopes
// fall back to the plain foreground color.
// Typed structurally: @shikijs/types is not a direct dependency, so the
// LanguageRegistration type is not importable under pnpm's isolation.
export const cliLang = {
  name: "cli",
  scopeName: "source.cli",
  patterns: [
    { match: "#.*$", name: "comment" },
    // Intentionally unmapped by the css-variables theme (which maps only
    // the exact scope `variable.parameter.function`), so placeholders
    // render in the standard foreground color.
    { match: "<[A-Za-z][A-Za-z0-9_-]*>", name: "variable.parameter" },
    { match: "\\b[A-Z][A-Z0-9_]*\\b", name: "variable.parameter" },
    { match: "(?<![\\w-])--?[A-Za-z][\\w-]*", name: "keyword" },
    { match: "^\\s*[a-z][\\w.-]*", name: "entity.name.function" },
    { match: "[\\[\\]]|\\.\\.\\.", name: "punctuation.separator" },
  ],
  repository: {},
};
