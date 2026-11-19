//! `bookclerk users`: the library's people. Today it reads and sets patron
//! scopes, which part of the library a Member sees.
//!
//! Reads and writes the library database directly, like `nodes`. The web
//! app's Settings → Users and `GET`/`PUT /api/users/{id}/scope` change the
//! same records.

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    assigned_patron_scope, default_patron_scope, role_is_scoped, set_default_patron_scope,
    set_patron_scope, ConfigActor, PatronScope, RuleEffect, ScopeField, ScopeRule,
};
use bookclerk_library::UserRecord;
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk users` verbs.
pub enum UsersCommand {
    /// Read or set which part of the library a patron sees.
    Scope {
        #[command(subcommand)]
        /// Scope verb.
        command: ScopeCommand,
    },
}

#[derive(Debug, Subcommand)]
/// `bookclerk users scope` verbs.
pub enum ScopeCommand {
    /// Print a patron's scope, or the library's default.
    Get {
        /// Login name or user id.
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        user: Option<String>,
        /// The library's default scope, which patrons without their own
        /// follow.
        #[arg(long)]
        default: bool,
    },
    /// Replace a patron's scope, or the library's default. With no
    /// `--include` or `--exclude`, the scope hides nothing.
    Set {
        /// Login name or user id.
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        user: Option<String>,
        /// Set the library's default scope instead of one patron's.
        #[arg(long)]
        default: bool,
        /// Show only books with this value (`genre=Kids`); fields are
        /// genre, series, author and narrator. Repeatable: a book needs
        /// one.
        #[arg(long, value_name = "FIELD=VALUE")]
        include: Vec<String>,
        /// Hide books with this value, whatever `--include` says.
        /// Repeatable.
        #[arg(long, value_name = "FIELD=VALUE")]
        exclude: Vec<String>,
        /// Drop the patron's own scope so they follow the library's default.
        #[arg(
            long,
            conflicts_with_all = ["default", "include", "exclude"]
        )]
        follow_default: bool,
    },
}

/// Runs a `users` command.
///
/// # Errors
///
/// Returns an error for an unknown user, an invalid rule, or a failed read
/// or write.
pub async fn run(
    command: UsersCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let UsersCommand::Scope { command } = command;
    match command {
        ScopeCommand::Get { user, default } => match (user, default) {
            (_, true) => print_default(format, &default_patron_scope(store).await?),
            (Some(user), false) => {
                let user = resolve_user(store, &user).await?;
                let assigned = assigned_patron_scope(store, user.id).await?;
                print_user(format, &user, &assigned.scope, assigned.own)
            }
            (None, false) => anyhow::bail!("name a user or pass --default"),
        },
        ScopeCommand::Set {
            user,
            default,
            include,
            exclude,
            follow_default,
        } => {
            let scope = if follow_default {
                None
            } else {
                Some(scope_from_args(&include, &exclude)?)
            };
            match (user, default) {
                (_, true) => {
                    let actor = ConfigActor::Operator { id: "cli".into() };
                    let stored =
                        set_default_patron_scope(store, &actor, scope.unwrap_or_default()).await?;
                    print_default(format, &stored.body)
                }
                (Some(user), false) => {
                    let user = resolve_user(store, &user).await?;
                    let assigned = set_patron_scope(store, user.id, scope).await?;
                    print_user(format, &user, &assigned.scope, assigned.own)
                }
                (None, false) => anyhow::bail!("name a user or pass --default"),
            }
        }
    }
}

/// Builds a scope from `--include` and `--exclude` values.
///
/// # Errors
///
/// Returns an error for a value without `=` or an unknown field.
fn scope_from_args(include: &[String], exclude: &[String]) -> anyhow::Result<PatronScope> {
    let mut rules = Vec::new();
    for (values, effect) in [
        (include, RuleEffect::Include),
        (exclude, RuleEffect::Exclude),
    ] {
        for raw in values {
            let Some((field, value)) = raw.split_once('=') else {
                anyhow::bail!("`{raw}` is not FIELD=VALUE");
            };
            rules.push(ScopeRule {
                field: parse_field(field)?,
                value: value.to_string(),
                effect,
            });
        }
    }
    Ok(PatronScope { rules })
}

/// Parses a rule field name.
fn parse_field(field: &str) -> anyhow::Result<ScopeField> {
    match field.trim().to_ascii_lowercase().as_str() {
        "genre" => Ok(ScopeField::Genre),
        "series" => Ok(ScopeField::Series),
        "author" => Ok(ScopeField::Author),
        "narrator" => Ok(ScopeField::Narrator),
        other => {
            anyhow::bail!("unknown scope field `{other}`: use genre, series, author or narrator")
        }
    }
}

/// The text form of one rule (`include genre=Kids`).
fn rule_line(rule: &ScopeRule) -> String {
    let effect = match rule.effect {
        RuleEffect::Include => "include",
        RuleEffect::Exclude => "exclude",
    };
    let field = match rule.field {
        ScopeField::Genre => "genre",
        ScopeField::Series => "series",
        ScopeField::Author => "author",
        ScopeField::Narrator => "narrator",
    };
    format!("{effect} {field}={}", rule.value)
}

/// Prints the rules of `scope`, or that it hides nothing.
fn print_rules(scope: &PatronScope) {
    if scope.rules.is_empty() {
        println!("  (no rules: every book is visible)");
    }
    for rule in &scope.rules {
        println!("  {}", rule_line(rule));
    }
}

/// Prints the library's default scope.
fn print_default(format: OutputFormat, scope: &PatronScope) -> anyhow::Result<()> {
    emit(format, &json!({ "scope": scope }), || {
        println!("Default patron scope");
        print_rules(scope);
    })
}

/// Prints a patron's scope.
fn print_user(
    format: OutputFormat,
    user: &UserRecord,
    scope: &PatronScope,
    own: bool,
) -> anyhow::Result<()> {
    let applies = role_is_scoped(user.role);
    emit(
        format,
        &json!({
            "user_id": user.id,
            "scope": scope,
            "own": own,
            "applies": applies,
        }),
        || {
            let source = if own { "own scope" } else { "library default" };
            println!("User {} ({source})", user.id);
            if !applies {
                println!(
                    "  not applied: a {} always sees the whole library",
                    user.role.as_str()
                );
            }
            print_rules(scope);
        },
    )
}

/// Finds a user by login name or id.
///
/// # Errors
///
/// Returns an error when no user matches, or when `user` is one user's
/// login name and another user's id.
async fn resolve_user(
    store: &bookclerk_library::LibraryStore,
    user: &str,
) -> anyhow::Result<UserRecord> {
    let user = user.trim();
    let by_login = store.get_user_by_login_name(user).await?;
    let by_id = match user.parse::<i64>() {
        Ok(id) => store.get_user(id).await?,
        Err(_) => None,
    };
    match (by_login, by_id) {
        (Some(login), Some(numbered)) if login.id != numbered.id => anyhow::bail!(
            "`{user}` is the login name of user {} and also user id {}; rename one",
            login.id,
            numbered.id
        ),
        (Some(found), _) | (None, Some(found)) => Ok(found),
        (None, None) => anyhow::bail!("no user with login name or id `{user}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_from_field_value_pairs() {
        let scope = scope_from_args(&["genre=Kids".into()], &["Series=Discworld".into()]).unwrap();
        assert_eq!(scope.rules.len(), 2);
        assert_eq!(rule_line(&scope.rules[0]), "include genre=Kids");
        assert_eq!(rule_line(&scope.rules[1]), "exclude series=Discworld");
        assert!(scope_from_args(&["Kids".into()], &[]).is_err());
        assert!(scope_from_args(&["shelf=Kids".into()], &[]).is_err());
    }
}
