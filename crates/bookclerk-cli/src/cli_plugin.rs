//! Build clap subcommands from plugin [`CliSchema`] and map matches → invoke args.

use bookclerk_plugin_host::{CliArgKind, CliArgSpec, CliCommandSpec, CliSchema};
use clap::{Arg, ArgAction, ArgMatches, Command, ValueHint};
use serde_json::{Map, Value};

/// Reserved host subcommands under `bookclerk plugins` (cannot be plugin ids).
pub const RESERVED_PLUGIN_SUBCOMMANDS: &[&str] = &[
    "list", "info", "diagnose", "enable", "disable", "search", "install", "help",
];

/// Convert a plugin CLI schema into a clap command named `plugin_id`.
#[must_use]
pub fn schema_to_command(plugin_id: &str, about: Option<&str>, schema: &CliSchema) -> Command {
    let mut cmd = Command::new(plugin_id.to_string());
    if let Some(about) = about {
        cmd = cmd.about(about.to_string());
    } else {
        cmd = cmd.about(format!("Commands from plugin `{plugin_id}`"));
    }
    cmd = cmd.subcommand_required(true).arg_required_else_help(true);
    for spec in &schema.commands {
        cmd = cmd.subcommand(command_spec_to_clap(spec));
    }
    cmd
}

fn command_spec_to_clap(spec: &CliCommandSpec) -> Command {
    let mut cmd = Command::new(spec.name.clone());
    if let Some(about) = &spec.about {
        cmd = cmd.about(about.clone());
    }
    for arg in &spec.args {
        cmd = cmd.arg(arg_spec_to_clap(arg));
    }
    cmd
}

fn arg_spec_to_clap(spec: &CliArgSpec) -> Arg {
    let mut arg = Arg::new(spec.name.clone());
    if let Some(about) = &spec.about {
        arg = arg.help(about.clone());
    }
    if spec.positional {
        arg = arg.required(spec.required);
        if let Some(default) = &spec.default {
            arg = arg.default_value(default.clone());
        }
    } else {
        let long = spec
            .long
            .clone()
            .unwrap_or_else(|| spec.name.replace('_', "-"));
        arg = arg.long(long);
        if let Some(short) = spec.short {
            arg = arg.short(short);
        }
        if spec.required && spec.default.is_none() {
            arg = arg.required(true);
        }
        if let Some(default) = &spec.default {
            arg = arg.default_value(default.clone());
        }
    }
    match spec.kind {
        CliArgKind::Bool => {
            arg = arg.action(ArgAction::SetTrue);
        }
        CliArgKind::Int => {
            arg = arg.value_parser(clap::value_parser!(i64));
        }
        CliArgKind::Path => {
            arg = arg.value_hint(ValueHint::AnyPath);
        }
        CliArgKind::String => {}
    }
    arg
}

/// Extract invoke args for `command` from clap matches under that subcommand.
pub fn matches_to_invoke_args(
    spec: &CliCommandSpec,
    matches: &ArgMatches,
) -> anyhow::Result<Map<String, Value>> {
    let mut args = Map::new();
    for arg in &spec.args {
        if !matches.contains_id(arg.name.as_str()) {
            if let Some(default) = &arg.default {
                args.insert(arg.name.clone(), value_from_string(arg.kind, default)?);
            }
            continue;
        }
        let value = match arg.kind {
            CliArgKind::Bool => Value::Bool(matches.get_flag(arg.name.as_str())),
            CliArgKind::Int => {
                let n = matches
                    .get_one::<i64>(arg.name.as_str())
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("missing int arg `{}`", arg.name))?;
                Value::Number(n.into())
            }
            CliArgKind::String | CliArgKind::Path => {
                let s = matches
                    .get_one::<String>(arg.name.as_str())
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing arg `{}`", arg.name))?;
                Value::String(s)
            }
        };
        args.insert(arg.name.clone(), value);
    }
    Ok(args)
}

fn value_from_string(kind: CliArgKind, raw: &str) -> anyhow::Result<Value> {
    Ok(match kind {
        CliArgKind::Bool => Value::Bool(matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )),
        CliArgKind::Int => Value::Number(
            raw.parse::<i64>()
                .map_err(|err| anyhow::anyhow!("invalid int default `{raw}`: {err}"))?
                .into(),
        ),
        CliArgKind::String | CliArgKind::Path => Value::String(raw.to_string()),
    })
}

/// Find a command spec by name.
#[must_use]
pub fn find_command<'a>(schema: &'a CliSchema, name: &str) -> Option<&'a CliCommandSpec> {
    schema.commands.iter().find(|c| c.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_host::CliArgSpec;

    #[test]
    fn builds_and_parses_ping() {
        let schema = CliSchema {
            commands: vec![CliCommandSpec {
                name: "ping".into(),
                about: Some("Probe".into()),
                args: vec![CliArgSpec {
                    name: "message".into(),
                    long: Some("message".into()),
                    short: None,
                    kind: CliArgKind::String,
                    required: false,
                    default: Some("hi".into()),
                    about: None,
                    positional: false,
                }],
            }],
        };
        let cmd = schema_to_command("echo", Some("Echo"), &schema);
        let matches = cmd
            .try_get_matches_from(["echo", "ping", "--message", "hello"])
            .unwrap();
        let sub = matches.subcommand().unwrap();
        assert_eq!(sub.0, "ping");
        let spec = find_command(&schema, "ping").unwrap();
        let args = matches_to_invoke_args(spec, sub.1).unwrap();
        assert_eq!(args.get("message").and_then(|v| v.as_str()), Some("hello"));
    }
}
