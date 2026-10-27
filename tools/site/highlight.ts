// Syntax colors for the docs' fenced code blocks, rendered by the Worker so
// pages need no client-side highlighter. It is a small one-pass scanner, not
// a parser: it marks comments, strings, numbers, keywords and a few
// language-specific names (shell commands and variables, TOML and YAML keys,
// JSON property names), and leaves everything else as plain text.
//
// The scanner reads doc text from the docs repository, so it is sans-IO and
// its output is checked by fuzz.test.ts: every input comes back as escaped
// text and `<span class="hl-…">` elements only, and removing the spans gives
// back the input exactly.

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
  /** A command: the first word of a shell command. */
  | "f";

/** How a language's source is scanned. */
type Mode =
  /** Shell sessions and scripts: commands, flags and `$VARIABLES`. */
  | "shell"
  /** TOML: table headers and keys. */
  | "toml"
  /** YAML: keys before `:`. */
  | "yaml"
  /** JSON: property names apart from string values. */
  | "json"
  /** A programming language: keywords only. */
  | "code";

/** The rules for one language. */
interface Grammar {
  /** How names at the start of a line or before `=` / `:` are treated. */
  mode: Mode;
  /** Prefixes that start a comment running to the end of the line. */
  lineComments: readonly string[];
  /** Opening and closing marks of a block comment, if the language has one. */
  blockComment: readonly [string, string] | null;
  /** Characters that open and close a string literal on one line. */
  quotes: string;
  /** Words shown as keywords; checked case-insensitively when `caseless`. */
  keywords: ReadonlySet<string>;
  /** Words shown as literals, like numbers. */
  literals: ReadonlySet<string>;
  /** Whether keywords match in any letter case (SQL). */
  caseless: boolean;
}

/** Literals most languages share. */
const COMMON_LITERALS = new Set(["true", "false", "null"]);

/** TypeScript and JavaScript keywords. */
const TS_KEYWORDS = new Set(
  "as async await break case catch class const continue default delete do else enum export extends finally for from function if implements import in instanceof interface let new of private protected public readonly return static super switch this throw try type typeof var void while yield".split(" "),
);

/** Rust keywords. */
const RUST_KEYWORDS = new Set("as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait type unsafe use where while".split(" "));

/** Python keywords. */
const PYTHON_KEYWORDS = new Set("and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield".split(" "));

/** SQL keywords, matched in any case. */
const SQL_KEYWORDS = new Set(
  "add alter and as asc by create default delete desc distinct drop exists from group having if in index insert into is join key left limit not null on or order primary references select set table union unique update values where with".split(" "),
);

/** Shell words that are syntax, not commands. */
const SHELL_KEYWORDS = new Set("if then else elif fi for in do done while until case esac function export local return".split(" "));

/** Shell words after which the next word is a command again. */
const COMMAND_PREFIXES = new Set(["then", "do", "else", "elif", "sudo", "time", "exec"]);

/** Grammar for each language name a fence may give, lower-cased. */
const GRAMMARS: ReadonlyMap<string, Grammar> = (() => {
  const none: ReadonlySet<string> = new Set();
  const shell: Grammar = { mode: "shell", lineComments: ["#"], blockComment: null, quotes: `"'`, keywords: SHELL_KEYWORDS, literals: none, caseless: false };
  const toml: Grammar = { mode: "toml", lineComments: ["#"], blockComment: null, quotes: `"'`, keywords: none, literals: new Set(["true", "false"]), caseless: false };
  const yaml: Grammar = { mode: "yaml", lineComments: ["#"], blockComment: null, quotes: `"'`, keywords: none, literals: new Set([...COMMON_LITERALS, "yes", "no", "on", "off"]), caseless: false };
  const json: Grammar = { mode: "json", lineComments: ["//"], blockComment: ["/*", "*/"], quotes: `"`, keywords: none, literals: COMMON_LITERALS, caseless: false };
  const ts: Grammar = { mode: "code", lineComments: ["//"], blockComment: ["/*", "*/"], quotes: "\"'`", keywords: TS_KEYWORDS, literals: new Set([...COMMON_LITERALS, "undefined"]), caseless: false };
  const rust: Grammar = { mode: "code", lineComments: ["//"], blockComment: ["/*", "*/"], quotes: `"`, keywords: RUST_KEYWORDS, literals: new Set(["true", "false", "None", "Some", "Ok", "Err"]), caseless: false };
  const python: Grammar = { mode: "code", lineComments: ["#"], blockComment: null, quotes: `"'`, keywords: PYTHON_KEYWORDS, literals: new Set(["True", "False", "None"]), caseless: false };
  const sql: Grammar = { mode: "code", lineComments: ["--"], blockComment: ["/*", "*/"], quotes: `'"`, keywords: SQL_KEYWORDS, literals: none, caseless: true };
  return new Map<string, Grammar>([
    ["bash", shell],
    ["sh", shell],
    ["shell", shell],
    ["console", shell],
    ["zsh", shell],
    ["toml", toml],
    ["yaml", yaml],
    ["yml", yaml],
    ["json", json],
    ["jsonc", json],
    ["ts", ts],
    ["typescript", ts],
    ["tsx", ts],
    ["js", ts],
    ["javascript", ts],
    ["rust", rust],
    ["rs", rust],
    ["python", python],
    ["py", python],
    ["sql", sql],
  ]);
})();

/**
 * Escapes text for an HTML element body.
 *
 * @param text - Raw text.
 * @returns The text with `& < > " '` replaced by entities.
 */
function escapeText(text: string): string {
  return text.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#39;");
}

/**
 * Whether a character can be part of a name.
 *
 * @param c - One character, or undefined past either end.
 * @param dash - Whether `-` counts (shell, TOML and YAML names).
 * @returns True for ASCII letters, digits, `_`, and `-` when `dash`.
 */
function isNameChar(c: string | undefined, dash: boolean): boolean {
  if (c === undefined) return false;
  return (c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || (c >= "0" && c <= "9") || c === "_" || (dash && c === "-");
}

/**
 * Whether a character is an ASCII digit.
 *
 * @param c - One character, or undefined.
 * @returns True for `0`–`9`.
 */
function isDigit(c: string | undefined): boolean {
  return c !== undefined && c >= "0" && c <= "9";
}

/**
 * Index just past a string literal that opens at `start`.
 *
 * Strings end at their closing quote or at the end of the line, so a stray
 * quote never colors the rest of a block.
 *
 * @param code - The block.
 * @param start - Index of the opening quote.
 * @param escapes - Whether a backslash escapes the next character.
 * @returns The end index (exclusive).
 */
function stringEnd(code: string, start: number, escapes: boolean): number {
  const quote = code[start];
  let i = start + 1;
  while (i < code.length && code[i] !== "\n") {
    if (escapes && code[i] === "\\" && i + 1 < code.length && code[i + 1] !== "\n") i += 2;
    else if (code[i] === quote) return i + 1;
    else i += 1;
  }
  return i;
}

/**
 * Colors one code block.
 *
 * @param code - The block's text, as written in the doc.
 * @param lang - The first word of the fence's info string.
 * @returns HTML: the text escaped, with marked tokens wrapped in
 *   `<span class="hl-<kind>">`; null when the language has no grammar.
 */
export function highlight(code: string, lang: string): string | null {
  const g = GRAMMARS.get(lang.toLowerCase());
  if (g === undefined) return null;
  const dash = g.mode === "shell" || g.mode === "toml" || g.mode === "yaml";
  const out: string[] = [];
  let plain = "";
  const flush = (): void => {
    if (plain !== "") out.push(escapeText(plain));
    plain = "";
  };
  const mark = (kind: TokenKind, text: string): void => {
    flush();
    out.push(`<span class="hl-${kind}">${escapeText(text)}</span>`);
  };
  /** Whether only spaces or tabs come before `i` on its line. */
  const atLineStart = (i: number): boolean => {
    let j = i - 1;
    while (j >= 0 && (code[j] === " " || code[j] === "\t")) j--;
    return j < 0 || code[j] === "\n";
  };
  /** Whether `i` starts a YAML key: at the start of a line or after a list item's `- `. */
  const atKeyStart = (i: number): boolean => {
    if (atLineStart(i)) return true;
    let j = i - 1;
    while (j >= 0 && (code[j] === " " || code[j] === "\t")) j--;
    return j < i - 1 && code[j] === "-" && atLineStart(j);
  };
  /** The next character after `i` that is not a space or tab. */
  const nextSolid = (i: number): string | undefined => {
    let j = i;
    while (code[j] === " " || code[j] === "\t") j++;
    return code[j];
  };
  // In shell mode, whether the next word starts a command.
  let commandNext = true;
  // Where the last search for a `${` closing brace stopped (see below).
  let braceScanEnd = -1;
  let i = 0;
  while (i < code.length) {
    const c = code[i] ?? "";
    if (c === "\n") {
      plain += c;
      commandNext = true;
      i += 1;
      continue;
    }
    // Shell `#` starts a comment only at the start of a word.
    const comment = g.lineComments.find((p) => code.startsWith(p, i) && (g.mode !== "shell" || i === 0 || /\s/.test(code[i - 1] ?? "")));
    if (comment !== undefined) {
      const end = code.indexOf("\n", i);
      const stop = end === -1 ? code.length : end;
      mark("c", code.slice(i, stop));
      i = stop;
      continue;
    }
    if (g.blockComment !== null && code.startsWith(g.blockComment[0], i)) {
      const end = code.indexOf(g.blockComment[1], i + g.blockComment[0].length);
      const stop = end === -1 ? code.length : end + g.blockComment[1].length;
      mark("c", code.slice(i, stop));
      i = stop;
      continue;
    }
    if (g.quotes.includes(c)) {
      // Single-quoted shell, TOML and YAML strings and SQL strings take no
      // backslash escapes.
      const literal = (c === "'" && (g.mode === "shell" || g.mode === "toml" || g.mode === "yaml")) || g.caseless;
      const stop = stringEnd(code, i, !literal);
      const text = code.slice(i, stop);
      const isKey = (g.mode === "json" || (g.mode === "yaml" && atKeyStart(i))) && nextSolid(stop) === ":";
      mark(isKey ? "v" : "s", text);
      i = stop;
      commandNext = false;
      continue;
    }
    if (g.mode === "toml" && c === "[" && atLineStart(i)) {
      const end = code.indexOf("\n", i);
      const line = code.slice(i, end === -1 ? code.length : end);
      const closer = line.startsWith("[[") ? "]]" : "]";
      const close = line.indexOf(closer, closer.length);
      if (close !== -1) {
        mark("k", line.slice(0, close + closer.length));
        i += close + closer.length;
        continue;
      }
    }
    if (g.mode === "shell" && c === "$" && (code[i + 1] === "{" || isNameChar(code[i + 1], false))) {
      let stop = i + 1;
      if (code[stop] === "{") {
        // The closing brace must be on this line. A scan stops at the first
        // `}` or newline, and a later `${` before that point stops there too,
        // so its result is reused: many unclosed `${` stay linear.
        if (stop >= braceScanEnd) {
          braceScanEnd = stop + 1;
          while (braceScanEnd < code.length && code[braceScanEnd] !== "}" && code[braceScanEnd] !== "\n") braceScanEnd++;
        }
        stop = code[braceScanEnd] === "}" ? braceScanEnd + 1 : stop + 1;
      } else {
        while (isNameChar(code[stop], false)) stop++;
      }
      mark("v", code.slice(i, stop));
      i = stop;
      commandNext = false;
      continue;
    }
    if (g.mode === "shell" && c === "-" && !isNameChar(code[i - 1], true) && (isNameChar(code[i + 1], false) || (code[i + 1] === "-" && isNameChar(code[i + 2], false)))) {
      let stop = i + 1;
      while (isNameChar(code[stop], true)) stop++;
      mark("v", code.slice(i, stop));
      i = stop;
      continue;
    }
    if (isDigit(c) && !isNameChar(code[i - 1], dash)) {
      let stop = i + 1;
      while (isNameChar(code[stop], false) || (code[stop] === "." && isDigit(code[stop + 1]))) stop++;
      mark("n", code.slice(i, stop));
      i = stop;
      commandNext = false;
      continue;
    }
    if (isNameChar(c, false) && !isNameChar(code[i - 1], dash)) {
      let stop = i + 1;
      while (isNameChar(code[stop], dash) || (g.mode === "toml" && code[stop] === "." && isNameChar(code[stop + 1], true))) stop++;
      const word = code.slice(i, stop);
      const key = g.caseless ? word.toLowerCase() : word;
      const after = nextSolid(stop);
      if (g.mode === "toml" && atLineStart(i) && after === "=") mark("v", word);
      else if (g.mode === "yaml" && atKeyStart(i) && after === ":") mark("v", word);
      else if (g.keywords.has(key)) mark("k", word);
      else if (g.literals.has(word)) mark("n", word);
      else if (g.mode === "shell" && commandNext && after !== "=") mark("f", word);
      else plain += word;
      if (g.mode === "shell") commandNext = COMMAND_PREFIXES.has(word);
      i = stop;
      continue;
    }
    if (g.mode === "shell") {
      // A pipe, `;`, `&&` or `||` starts the next command; a `$ ` prompt
      // at the start of a line is not one.
      if (c === "|" || c === ";" || c === "&" || c === "(") commandNext = true;
      else if (!(c === " " || c === "\t" || (c === "$" && atLineStart(i)))) commandNext = false;
    }
    plain += c;
    i += 1;
  }
  flush();
  return out.join("");
}
