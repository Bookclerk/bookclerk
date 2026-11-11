//! The `cli` entrypoint: `ping` plus the notes commands on the `DB` binding.

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    DatabaseBinding, PluginCli, PluginError,
};

use crate::notes::{add_notes, count_notes, note_too_large, NotesReader};

/// Largest `--count` `notes-add` accepts, so one command stays a modest write.
pub const MAX_NOTES_PER_ADD: usize = 10_000;

/// Largest `--limit` `notes-list` accepts.
///
/// The listing goes back in one CLI reply, so it is bounded even though
/// [`NotesReader`] itself streams.
pub const MAX_NOTES_PER_LIST: usize = 1_000;

/// One named, non-positional CLI argument.
fn arg(name: &str, kind: CliArgKind, required: bool, default: Option<&str>) -> CliArgSpec {
    CliArgSpec {
        name: name.into(),
        long: Some(name.into()),
        short: None,
        kind,
        required,
        default: default.map(Into::into),
        about: None,
        positional: false,
    }
}

/// CLI schema advertised at `PluginCli.describe`.
#[must_use]
pub fn cli_schema() -> CliSchema {
    CliSchema {
        commands: vec![
            CliCommandSpec {
                name: "ping".into(),
                about: Some("Probe echo plugin".into()),
                args: vec![CliArgSpec {
                    short: Some("m".into()),
                    about: Some("Message to echo".into()),
                    ..arg("message", CliArgKind::String, false, Some("hi"))
                }],
            },
            CliCommandSpec {
                name: "notes-add".into(),
                about: Some("Store a note COUNT times in the plugin's own database".into()),
                args: vec![
                    arg("body", CliArgKind::String, true, None),
                    arg("count", CliArgKind::Int, false, Some("1")),
                ],
            },
            CliCommandSpec {
                name: "notes-list".into(),
                about: Some("List the oldest LIMIT stored notes".into()),
                args: vec![arg("limit", CliArgKind::Int, false, Some("100"))],
            },
        ],
    }
}

/// Parses a whole-number CLI argument within `1..=maximum`.
///
/// # Returns
///
/// The number, or `None` when `value` is not one to six plain decimal digits
/// in range.
#[must_use]
pub fn whole_number(value: &str, maximum: usize) -> Option<usize> {
    if value.is_empty() || value.len() > 6 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = value.parse().ok()?;
    (1..=maximum).contains(&n).then_some(n)
}

/// A CLI reply with `exit_code`, `stdout` and `stderr`.
fn reply(exit_code: i32, stdout: String, stderr: String) -> CliInvokeResult {
    CliInvokeResult {
        exit_code,
        stdout,
        stderr,
        ..CliInvokeResult::default()
    }
}

/// A CLI reply for a failed database call.
///
/// A typed limit names the cap (exit status 1); any other error is passed on
/// so the host reports it, keeping its code.
///
/// # Arguments
///
/// * `err` - What the binding returned.
/// * `committed` - Notes already stored by batches that committed before the
///   failure; they stay stored.
///
/// # Errors
///
/// Returns `err` itself when it is not a `limit_exceeded` error and nothing
/// was stored; otherwise an error with `err`'s code and details whose
/// message says how many notes were.
pub fn database_failure(
    err: PluginError,
    committed: usize,
) -> Result<CliInvokeResult, PluginError> {
    let kept = (committed > 0).then(|| format!("{committed} notes were stored before the failure"));
    let Some(limit) = err.limit() else {
        return Err(match kept {
            None => err,
            Some(kept) => {
                let mut wrapped = err;
                wrapped.message = format!("{kept}: {}", wrapped.message);
                wrapped
            }
        });
    };
    let mut stderr = format!(
        "the database refused the request: {} is {}, it needed {}\n",
        limit.limit, limit.cap, limit.used
    );
    if let Some(kept) = kept {
        stderr.push_str(&kept);
        stderr.push('\n');
    }
    Ok(reply(1, String::new(), stderr))
}

/// The `cli` entrypoint, holding the `DB` binding the host granted.
pub struct NotesCli {
    /// The notes binding, `None` when the host granted none.
    db: Option<DatabaseBinding>,
}

impl NotesCli {
    /// The entrypoint over `db`.
    #[must_use]
    pub fn new(db: Option<DatabaseBinding>) -> Self {
        Self { db }
    }

    /// The granted binding.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` when the host granted no `DB` binding.
    fn db(&self) -> Result<&DatabaseBinding, PluginError> {
        self.db
            .as_ref()
            .ok_or_else(|| PluginError::unavailable("the DB binding was not granted"))
    }

    /// Stores `--body` `--count` times and reports the new total.
    async fn notes_add(&self, params: &CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        let Some(count) = whole_number(arg_value(params, "count", "1"), MAX_NOTES_PER_ADD) else {
            return Ok(reply(
                2,
                String::new(),
                format!("--count must be 1 to {MAX_NOTES_PER_ADD}\n"),
            ));
        };
        let db = self.db()?;
        let body = arg_value(params, "body", "");
        // `add_notes` checks every body too; checking the one body here
        // refuses an oversized note before it is copied `count` times.
        let cell_cap = match db.limits().await {
            Ok(caps) => u64::from(caps.max_cell_bytes),
            Err(err) => return database_failure(err, 0),
        };
        let used = u64::try_from(body.len()).unwrap_or(u64::MAX);
        if used > cell_cap {
            return database_failure(note_too_large(used, cell_cap), 0);
        }
        let bodies = vec![body.to_owned(); count];
        let mut committed = 0;
        let stored = match add_notes(db, &bodies, |n| committed = n).await {
            Ok(stored) => stored,
            Err(err) => return database_failure(err, committed),
        };
        let total = match count_notes(db).await {
            Ok(total) => total,
            Err(err) => return database_failure(err, committed),
        };
        Ok(reply(
            0,
            format!("stored {stored}; {total} in total\n"),
            String::new(),
        ))
    }

    /// Prints the oldest `--limit` notes, one `id: body` line each.
    async fn notes_list(&self, params: &CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        let Some(limit) = whole_number(arg_value(params, "limit", "100"), MAX_NOTES_PER_LIST)
        else {
            return Ok(reply(
                2,
                String::new(),
                format!("--limit must be 1 to {MAX_NOTES_PER_LIST}\n"),
            ));
        };
        let mut notes = NotesReader::new(self.db()?, Some(limit));
        let mut stdout = String::new();
        while let Some(note) = notes.next().await {
            match note {
                Ok(note) => stdout.push_str(&format!("{}: {}\n", note.id, note.body)),
                Err(err) => return database_failure(err, 0),
            }
        }
        Ok(reply(0, stdout, String::new()))
    }
}

/// The value of argument `name`, or `default` when it was not given.
fn arg_value<'a>(params: &'a CliInvokeParams, name: &str, default: &'a str) -> &'a str {
    params
        .args
        .iter()
        .find(|arg| arg.name == name)
        .map_or(default, |arg| arg.value.as_str())
}

#[async_trait(?Send)]
impl PluginCli for NotesCli {
    async fn describe(&self) -> Result<CliSchema, PluginError> {
        Ok(cli_schema())
    }

    async fn invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        match params.command.as_str() {
            "ping" => Ok(reply(
                0,
                format!("pong: {}\n", arg_value(&params, "message", "hi")),
                String::new(),
            )),
            "notes-add" => self.notes_add(&params).await,
            "notes-list" => self.notes_list(&params).await,
            other => Ok(reply(
                2,
                String::new(),
                format!("unknown command {other}\n"),
            )),
        }
    }
}
