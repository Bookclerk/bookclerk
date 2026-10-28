// Syntax colors for the docs' fenced code blocks, rendered by the Worker so
// pages need no client-side highlighter. Shiki tokenizes the code with the
// same TextMate grammars editors use (core, the JavaScript regex engine, and
// only the languages the docs use, so the Worker bundle stays small).
//
// Shiki's own HTML puts a color in a `style` attribute on every token, which
// the site's Content-Security-Policy forbids. So the theme below gives each
// kind of token a placeholder color, and this module turns those back into
// the site's `hl-*` classes and writes the HTML itself.
//
// The highlighter reads doc text from the docs repository, so its output is
// checked by fuzz.test.ts: every input comes back as escaped text and
// `<span class="hl-…">` elements only, and removing the spans gives back the
// input exactly.

import {
  createHighlighterCoreSync,
  type HighlighterCore,
  type ThemeRegistration,
} from "@shikijs/core";
import { createJavaScriptRegexEngine } from "@shikijs/engine-javascript";
import http from "@shikijs/langs/http";
import json from "@shikijs/langs/json";
import jsonc from "@shikijs/langs/jsonc";
import nginx from "@shikijs/langs/nginx";
import powershell from "@shikijs/langs/powershell";
import python from "@shikijs/langs/python";
import rust from "@shikijs/langs/rust";
import shellscript from "@shikijs/langs/shellscript";
import sql from "@shikijs/langs/sql";
import toml from "@shikijs/langs/toml";
import typescript from "@shikijs/langs/typescript";
import yaml from "@shikijs/langs/yaml";

/** What a marked span is; its class is `hl-<kind>`. */
export type TokenKind =
  /** A comment. */
  | "c"
  /** A string literal. */
  | "s"
  /** A number, or a literal such as `true` or `null`. */
  | "n"
  /** A language keyword, or a TOML table header. */
  | "k"
  /** A name the reader looks for: a key, a variable or a command-line flag. */
  | "v"
  /** A command or function name. */
  | "f";

/** Placeholder colors the theme gives each kind; never shown to readers. */
const KIND_COLORS: Readonly<Record<TokenKind, string>> = {
  c: "#000001",
  s: "#000002",
  n: "#000003",
  k: "#000004",
  v: "#000005",
  f: "#000006",
};

/** The kind each placeholder color stands for. */
const COLOR_KINDS: ReadonlyMap<string, TokenKind> = new Map(
  (Object.keys(KIND_COLORS) as TokenKind[]).map((kind) => [KIND_COLORS[kind].toLowerCase(), kind]),
);

/** Name of the placeholder theme. */
const THEME = "bookclerk";

/**
 * Which TextMate scopes get which kind (null: plain). A token takes the rule
 * whose scope matches most specifically, so quotes inside a string stay part
 * of the string, and `keyword.operator` (`|`, `=`) and a shell line's final
 * `\` stay plain.
 */
const THEME_RULES: readonly [TokenKind | null, readonly string[]][] = [
  ["c", ["comment"]],
  [
    "s",
    ["string.quoted", "string.interpolated", "string.regexp", "string.template", "string.other"],
  ],
  ["n", ["constant.numeric", "constant.language", "constant.character"]],
  [
    "k",
    [
      "keyword",
      "storage",
      "entity.name.section",
      "punctuation.definition.section",
      "support.type.primitive",
    ],
  ],
  [null, ["keyword.operator", "constant.character.escape.line-continuation"]],
  [
    "v",
    [
      "variable.other.normal",
      "variable.other.positional",
      "variable.other.special",
      "variable.other.assignment",
      "variable.other.key",
      "punctuation.definition.variable",
      "constant.other.option",
      "support.type.property-name",
      "entity.name.tag",
    ],
  ],
  ["f", ["entity.name.command", "entity.name.function", "support.function"]],
];

/** The placeholder theme: plain text is black, each kind its own color. */
const THEME_REGISTRATION: ThemeRegistration = {
  name: THEME,
  type: "light",
  fg: "#000000",
  bg: "#ffffff",
  settings: [
    { settings: { foreground: "#000000" } },
    ...THEME_RULES.map(([kind, scope]) => ({
      scope: [...scope],
      settings: { foreground: kind === null ? "#000000" : KIND_COLORS[kind] },
    })),
  ],
};

/**
 * Grammar for each language name a fence may give, lower-cased. `console`
 * is a terminal session: shell commands after `$ ` prompts, plain output.
 */
const LANGUAGE_NAMES: ReadonlyMap<string, string> = new Map([
  ["bash", "shellscript"],
  ["sh", "shellscript"],
  ["shell", "shellscript"],
  ["zsh", "shellscript"],
  ["console", "shellscript"],
  ["toml", "toml"],
  ["yaml", "yaml"],
  ["yml", "yaml"],
  ["json", "json"],
  ["jsonc", "jsonc"],
  ["ts", "typescript"],
  ["typescript", "typescript"],
  ["tsx", "typescript"],
  ["js", "typescript"],
  ["javascript", "typescript"],
  ["rust", "rust"],
  ["rs", "rust"],
  ["python", "python"],
  ["py", "python"],
  ["powershell", "powershell"],
  ["pwsh", "powershell"],
  ["ps1", "powershell"],
  ["sql", "sql"],
  ["nginx", "nginx"],
  ["http", "http"],
]);

/** The highlighter, created on first use so Worker start-up stays quick. */
let highlighter: HighlighterCore | null = null;

/**
 * Returns the highlighter, creating it on the first call.
 *
 * @returns The Shiki highlighter with the docs' languages and the placeholder theme.
 */
function getHighlighter(): HighlighterCore {
  highlighter ??= createHighlighterCoreSync({
    themes: [THEME_REGISTRATION],
    langs: [
      http,
      json,
      jsonc,
      nginx,
      powershell,
      python,
      rust,
      shellscript,
      sql,
      toml,
      typescript,
      yaml,
    ],
    // A grammar pattern the engine cannot translate matches nothing instead of throwing.
    engine: createJavaScriptRegexEngine({ forgiving: true }),
  });
  return highlighter;
}

/** Tokenizer state carried from one line to the next. */
type GrammarState = ReturnType<HighlighterCore["getLastGrammarState"]>;

/**
 * Escapes text for an HTML element body.
 *
 * @param text - Raw text.
 * @returns The text with `& < > " '` replaced by entities.
 */
function escapeText(text: string): string {
  return text
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/** One line's colored HTML, and what the tokenizer needs for the next line. */
interface Line {
  /** The line as escaped text and `hl-*` spans. */
  html: string;
  /** Tokenizer state after the line, when the tokenizer reports one. */
  state: GrammarState | undefined;
  /** Whether the line ends inside a comment (so a final `\` is commentary). */
  endsInComment: boolean;
}

/**
 * Colors one line, carrying the tokenizer state across lines so a string or
 * block comment that spans lines stays colored.
 *
 * @param shiki - The highlighter.
 * @param text - One line, without its newline.
 * @param lang - The Shiki language id.
 * @param state - Tokenizer state after the previous line, if any.
 * @returns The line's HTML and state, or null when the tokens do not add up
 *   to the line exactly (the block is then left plain).
 */
function highlightLine(
  shiki: HighlighterCore,
  text: string,
  lang: string,
  state: GrammarState | undefined,
): Line | null {
  const result = shiki.codeToTokens(text, {
    lang,
    theme: THEME,
    ...(state === undefined ? {} : { grammarState: state }),
  });
  if (result.tokens.length > 1) return null;
  let html = "";
  let seen = "";
  let open: TokenKind | null = null;
  let last: TokenKind | null = null;
  for (const token of result.tokens[0] ?? []) {
    if (token.content === "") continue;
    seen += token.content;
    const kind = COLOR_KINDS.get((token.color ?? "").toLowerCase()) ?? null;
    if (kind !== open) {
      if (open !== null) html += "</span>";
      if (kind !== null) html += `<span class="hl-${kind}">`;
      open = kind;
    }
    html += escapeText(token.content);
    last = kind;
  }
  if (open !== null) html += "</span>";
  if (seen !== text) return null;
  return { html, state: result.grammarState ?? state, endsInComment: last === "c" };
}

/**
 * Whether a shell line continues onto the next: it ends in an odd run of
 * backslashes (an even run is escaped backslashes) outside a comment.
 *
 * @param text - The line.
 * @param endsInComment - Whether the tokenizer ended the line in a comment.
 * @returns True when the next line is part of the same command.
 */
function continues(text: string, endsInComment: boolean): boolean {
  let run = 0;
  while (text[text.length - 1 - run] === "\\") run++;
  return run % 2 === 1 && !endsInComment;
}

/**
 * Colors a terminal session: lines after a `$ ` prompt, and the lines they
 * continue onto, are shell commands; every other line is output and stays
 * plain.
 *
 * @param shiki - The highlighter.
 * @param lines - The block's lines.
 * @returns The HTML, or null when a command line cannot be colored exactly.
 */
function highlightSession(shiki: HighlighterCore, lines: readonly string[]): string | null {
  const out: string[] = [];
  let command = false;
  let state: GrammarState | undefined;
  for (const text of lines) {
    if (!command && !text.startsWith("$ ")) {
      out.push(escapeText(text));
      state = undefined;
      continue;
    }
    const prompt = command ? "" : "$ ";
    const line = highlightLine(shiki, text.slice(prompt.length), "shellscript", state);
    if (line === null) return null;
    out.push(prompt + line.html);
    command = continues(text, line.endsInComment);
    state = command ? line.state : undefined;
  }
  return out.join("\n");
}

/**
 * Colors one code block.
 *
 * @param code - The fence's contents.
 * @param lang - The fence's language name, in any letter case.
 * @returns HTML: the escaped code with `<span class="hl-…">` elements around
 *   the parts the grammar recognizes; null when the language is not one the
 *   site colors, or the code cannot be colored exactly, so the block stays
 *   plain.
 */
export function highlight(code: string, lang: string): string | null {
  const name = lang.toLowerCase();
  const id = LANGUAGE_NAMES.get(name);
  if (id === undefined) return null;
  try {
    const shiki = getHighlighter();
    const lines = code.split("\n");
    if (name === "console") return highlightSession(shiki, lines);
    const out: string[] = [];
    let state: GrammarState | undefined;
    for (const text of lines) {
      const line = highlightLine(shiki, text, id, state);
      if (line === null) return null;
      out.push(line.html);
      state = line.state;
    }
    return out.join("\n");
  } catch {
    return null;
  }
}
