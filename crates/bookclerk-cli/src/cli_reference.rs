//! Renders the `bookclerk` command tree as the Markdown CLI reference.
//!
//! The reference lives at `docs/server/cli-reference.md` and is generated from the
//! same clap definitions the binary parses, so it cannot describe a flag the
//! CLI does not have. The `cli_reference_is_current` test fails when the file
//! drifts; run it with `UPDATE_CLI_REFERENCE=1` to rewrite the file.

use std::fmt::Write as _;

use clap::{Arg, Command};

/// Path of the generated reference, relative to the workspace root.
pub const REFERENCE_PATH: &str = "docs/server/cli-reference.md";

/// Text placed above the generated sections.
///
/// It names the generator so nobody edits the file by hand, and explains the
/// parts of the CLI the static tree cannot show (plugin subcommands).
const PREAMBLE: &str = "\
# CLI reference

Every built-in command and option of the `bookclerk` command-line tool. During
development, run it as `cargo dev-cli -- <command>` or
`cargo run -p bookclerk-cli -- <command>`. `bookclerk <command> --help`
prints the same text in a terminal.

Installed plugins can add their own commands under
`bookclerk plugins <plugin-id>`. Those depend on which plugins are installed,
so they are not listed here; `bookclerk plugins <plugin-id> --help` shows
them.

This page is generated from the CLI's own definitions, so do not edit it by
hand. After changing a command, run
`UPDATE_CLI_REFERENCE=1 cargo test -p bookclerk-cli cli_reference` and commit
the result.
";

/// Renders the whole reference for a fully built root command.
///
/// # Arguments
///
/// * `root` - the `bookclerk` command, after [`Command::build`], so global
///   options are propagated and every subcommand has its full bin name.
///
/// # Returns
///
/// The complete Markdown file, ending in one newline.
pub fn render(root: &Command) -> String {
    let mut out = String::from(PREAMBLE);
    out.push_str("\n## Global options\n\n");
    let globals: Vec<&Arg> = root
        .get_arguments()
        .filter(|arg| is_documented_option(arg))
        .collect();
    let mut shadowing = Vec::new();
    collect_shadowing(root, &globals, &mut shadowing);
    if shadowing.is_empty() {
        out.push_str("These options work with every command.\n\n");
    } else {
        let _ = writeln!(
            out,
            "These options work with every command. Some commands have their own option \
             of the same name: written after the command's name, it means the command's \
             option, and written before it, the global one. These are {}.\n",
            shadowing.join(", ")
        );
    }
    render_args(&mut out, &globals);
    out.push_str("## Commands\n\n");
    render_command_list(&mut out, root);
    for sub in visible_subcommands(root) {
        render_command(&mut out, sub, 2);
    }
    // Every section ends in a blank line; the file ends in exactly one newline.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Finds subcommand options that reuse a global option's long name.
///
/// clap lets a command's own option shadow a global one, so for that command
/// the global option does not apply (for example `export postgres --format`
/// picks the destination schema, not the output format).
///
/// # Arguments
///
/// * `cmd` - the command whose subcommands are searched, recursively.
/// * `globals` - the root's documented global options.
/// * `found` - receives each shadowing option as `` `bookclerk … --name` ``.
fn collect_shadowing(cmd: &Command, globals: &[&Arg], found: &mut Vec<String>) {
    for sub in visible_subcommands(cmd) {
        let name = sub.get_bin_name().unwrap_or_else(|| sub.get_name());
        for arg in sub.get_arguments().filter(|arg| !arg.is_global_set()) {
            if let Some(long) = arg.get_long() {
                if globals.iter().any(|global| global.get_long() == Some(long)) {
                    found.push(format!("`{name} --{long}`"));
                }
            }
        }
        collect_shadowing(sub, globals, found);
    }
}

/// Appends one command's section, then the sections of its subcommands.
///
/// # Arguments
///
/// * `out` - the Markdown being built.
/// * `cmd` - the command to describe.
/// * `level` - Markdown heading level: 2 for top-level commands, one more
///   for each level of nesting, at most 6.
fn render_command(out: &mut String, cmd: &Command, level: usize) {
    let name = cmd.get_bin_name().unwrap_or_else(|| cmd.get_name());
    let _ = writeln!(out, "{} `{name}`\n", "#".repeat(level));
    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        let _ = writeln!(out, "{}\n", prose(about.to_string().trim()));
    }
    let aliases: Vec<&str> = cmd.get_visible_aliases().collect();
    if !aliases.is_empty() {
        let _ = writeln!(out, "Also available as `{}`.\n", aliases.join("`, `"));
    }
    let usage = cmd.clone().render_usage().to_string();
    let usage = usage.trim().trim_start_matches("Usage:").trim();
    let _ = writeln!(out, "```text\n{usage}\n```\n");

    let positionals: Vec<&Arg> = cmd
        .get_positionals()
        .filter(|arg| !arg.is_hide_set())
        .collect();
    if !positionals.is_empty() {
        out.push_str("Arguments:\n\n");
        render_args(out, &positionals);
    }
    let options: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|arg| is_documented_option(arg) && !arg.is_global_set())
        .collect();
    if !options.is_empty() {
        out.push_str("Options:\n\n");
        render_args(out, &options);
    }
    if visible_subcommands(cmd).next().is_some() {
        out.push_str("Commands:\n\n");
        render_command_list(out, cmd);
    }
    for sub in visible_subcommands(cmd) {
        render_command(out, sub, (level + 1).min(6));
    }
}

/// Appends a bullet list linking to each visible subcommand's section.
///
/// # Arguments
///
/// * `out` - the Markdown being built.
/// * `cmd` - the parent command whose subcommands are listed.
fn render_command_list(out: &mut String, cmd: &Command) {
    for sub in visible_subcommands(cmd) {
        let sub_name = sub.get_bin_name().unwrap_or_else(|| sub.get_name());
        let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        let _ = writeln!(
            out,
            "- [`{sub_name}`](#{}): {}",
            anchor(sub_name),
            prose(&about)
        );
    }
    out.push('\n');
}

/// Appends a bullet list describing each argument.
///
/// # Arguments
///
/// * `out` - the Markdown being built.
/// * `args` - the arguments, in the order clap declares them.
fn render_args(out: &mut String, args: &[&Arg]) {
    for arg in args {
        let help = arg
            .get_long_help()
            .or_else(|| arg.get_help())
            .map(|h| h.to_string())
            .unwrap_or_default();
        // Join wrapped help lines so each option stays one list item.
        let help = prose(&help.split_whitespace().collect::<Vec<_>>().join(" "));
        let _ = write!(out, "- `{}`", arg_signature(arg));
        if !help.is_empty() {
            let _ = write!(out, ": {help}");
        }
        // Notes the help text already states are left out.
        let lower_help = help.to_lowercase();
        let mut notes = Vec::new();
        if arg.is_required_set() && !arg.is_positional() && !lower_help.contains("(required") {
            notes.push("required".to_owned());
        }
        if is_repeatable(arg) && !arg.is_positional() && !lower_help.contains("repeatable") {
            notes.push("can be repeated".to_owned());
        }
        let values: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|v| !v.is_hide_set())
            .map(|v| format!("`{}`", v.get_name()))
            .collect();
        if !values.is_empty() && !arg.is_hide_possible_values_set() {
            notes.push(format!("one of {}", values.join(", ")));
        }
        // Switches and counters always default to off; only values are worth noting.
        let defaults: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|v| format!("`{}`", v.to_string_lossy()))
            .collect();
        let help_states_default = lower_help.contains("default");
        if !defaults.is_empty()
            && (arg.is_positional() || takes_value(arg))
            && !arg.is_hide_default_value_set()
            && !help_states_default
        {
            notes.push(format!("default {}", defaults.join(", ")));
        }
        if let Some(env) = arg.get_env().filter(|_| !arg.is_hide_env_set()) {
            notes.push(format!("environment variable `{}`", env.to_string_lossy()));
        }
        if !notes.is_empty() {
            let _ = write!(out, " ({})", notes.join("; "));
        }
        out.push('\n');
    }
    out.push('\n');
}

/// Formats how an argument is written on the command line.
///
/// # Arguments
///
/// * `arg` - the argument to format.
///
/// # Returns
///
/// For example `-v, --verbose`, `--asin <ASIN>`, or, for positional
/// arguments, `<PLUGIN>` when required and `[ACCOUNT]...` when optional and
/// repeatable, as in the usage line.
fn arg_signature(arg: &Arg) -> String {
    let value = arg
        .get_value_names()
        .map(|names| {
            names
                .iter()
                .map(|n| format!("<{n}>"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| format!("<{}>", arg.get_id().as_str().to_uppercase()));
    if arg.is_positional() {
        let value = if arg.is_required_set() {
            value
        } else {
            format!("[{}]", value.trim_start_matches('<').trim_end_matches('>'))
        };
        return if is_repeatable(arg) {
            format!("{value}...")
        } else {
            value
        };
    }
    let mut names = Vec::new();
    if let Some(short) = arg.get_short() {
        names.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        names.push(format!("--{long}"));
    }
    if let Some(aliases) = arg.get_visible_aliases() {
        names.extend(aliases.iter().map(|a| format!("--{a}")));
    }
    let mut signature = names.join(", ");
    if takes_value(arg) {
        signature.push(' ');
        signature.push_str(&value);
    }
    signature
}

/// Says whether an option is followed by a value on the command line.
///
/// # Arguments
///
/// * `arg` - a named (non-positional) argument.
///
/// # Returns
///
/// `true` for options such as `--asin <ASIN>`, `false` for switches and
/// counters such as `--scan` or `-v`.
fn takes_value(arg: &Arg) -> bool {
    arg.get_action().takes_values()
}

/// Says whether an argument may be given more than once.
///
/// # Arguments
///
/// * `arg` - an argument of a built command.
///
/// # Returns
///
/// `true` when the argument collects several values, either from one
/// occurrence (`[ACCOUNT]...`) or from repeating the option.
fn is_repeatable(arg: &Arg) -> bool {
    matches!(arg.get_action(), clap::ArgAction::Append)
        || arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1)
}

/// Turns clap help text into Markdown prose.
///
/// Help strings come from doc comments, so they can hold rustdoc links such
/// as ``[`SCHEMA_VERSION`]`` and placeholders such as `<dir>`. Outside code
/// spans, this unwraps those links to plain code and escapes `<`, so neither
/// GitHub nor the docs site swallows or links them.
///
/// # Arguments
///
/// * `text` - help text as clap stores it.
///
/// # Returns
///
/// The text, safe to place in a Markdown paragraph or list item.
fn prose(text: &str) -> String {
    let unlinked = text.replace("[`", "`").replace("`]", "`");
    let mut out = String::with_capacity(unlinked.len());
    let mut in_code = false;
    for c in unlinked.chars() {
        match c {
            '`' => {
                in_code = !in_code;
                out.push(c);
            }
            '<' if !in_code => out.push_str("&lt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Says whether a named option belongs in the reference.
///
/// # Arguments
///
/// * `arg` - an argument of some command.
///
/// # Returns
///
/// `false` for positionals, hidden options, and clap's own `--help` and
/// `--version`, which every command has.
fn is_documented_option(arg: &Arg) -> bool {
    !arg.is_positional()
        && !arg.is_hide_set()
        && !matches!(arg.get_id().as_str(), "help" | "version")
}

/// Lists the subcommands a user can see in `--help`.
///
/// # Arguments
///
/// * `cmd` - the parent command.
///
/// # Returns
///
/// Every subcommand except hidden aliases and clap's generated `help`.
fn visible_subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

/// Computes the heading anchor the docs site gives a command's section.
///
/// # Arguments
///
/// * `bin_name` - the command's full name, such as `bookclerk library scan`.
///
/// # Returns
///
/// The lowercase, hyphen-joined anchor, such as `bookclerk-library-scan`.
fn anchor(bin_name: &str) -> String {
    bin_name
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::CommandFactory;

    use super::{render, REFERENCE_PATH};
    use crate::Cli;

    /// Fails when `docs/server/cli-reference.md` no longer matches the CLI.
    ///
    /// With `UPDATE_CLI_REFERENCE=1` it rewrites the file instead.
    #[test]
    fn cli_reference_is_current() {
        let mut root = Cli::command();
        root.build();
        let rendered = render(&root);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(REFERENCE_PATH);
        if std::env::var_os("UPDATE_CLI_REFERENCE").is_some() {
            std::fs::write(&path, &rendered).expect("write CLI reference");
            return;
        }
        // Windows checkouts may convert the file to CRLF line endings.
        let current = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
            .replace("\r\n", "\n");
        assert!(
            current == rendered,
            "{REFERENCE_PATH} is out of date with the CLI definitions; run \
             `UPDATE_CLI_REFERENCE=1 cargo test -p bookclerk-cli cli_reference` \
             and commit the result"
        );
    }
}
