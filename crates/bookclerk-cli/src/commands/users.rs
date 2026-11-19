//! `bookclerk users`: the library's people. Today it reads and sets patron
//! scopes, which part of the library a Member or Host sees, and member
//! controls, what a Member may do.
//!
//! Reads and writes the library database directly, like `nodes`. The web
//! app's Settings → Users, `GET`/`PUT /api/users/{id}/scope` and
//! `GET`/`PUT /api/users/{id}/controls` change the same records.

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    assigned_member_controls, assigned_patron_scope, default_member_controls, default_patron_scope,
    member_control_overrides, role_follows_default_controls, role_has_controls, role_is_scoped,
    set_default_member_controls, set_default_patron_scope, set_member_control_overrides,
    set_patron_scope, AssignedControls, ConfigActor, MemberControl, MemberControls, PatronScope,
    RuleEffect, ScopeField, ScopeRule,
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
    /// Read or set what a patron may do: link store accounts, download,
    /// edit metadata, upload, request books, see prices.
    Controls {
        #[command(subcommand)]
        /// Controls verb.
        command: ControlsCommand,
    },
}

#[derive(Debug, Subcommand)]
/// `bookclerk users controls` verbs.
pub enum ControlsCommand {
    /// Print a patron's controls, or the library's default.
    Get {
        /// Login name or user id.
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        user: Option<String>,
        /// The library's default, which patrons follow unless a control is
        /// set for them alone.
        #[arg(long)]
        default: bool,
    },
    /// Change controls of a patron, or of the library's default. Controls
    /// not named keep their value.
    Set {
        /// Login name or user id.
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        user: Option<String>,
        /// Change the library's default instead of one patron's controls.
        #[arg(long)]
        default: bool,
        /// Allow a control (`link_accounts`, `download`, `edit_metadata`,
        /// `upload`, `request_books`, `see_prices`). Repeatable.
        #[arg(long, value_name = "CONTROL")]
        allow: Vec<String>,
        /// Turn a control off. Repeatable.
        #[arg(long, value_name = "CONTROL")]
        deny: Vec<String>,
        /// Make the patron follow the library's default for a control
        /// again. Repeatable; `all` names every control.
        #[arg(long, value_name = "CONTROL", conflicts_with = "default")]
        follow_default: Vec<String>,
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
    let command = match command {
        UsersCommand::Scope { command } => command,
        UsersCommand::Controls { command } => return run_controls(store, command, format).await,
    };
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

/// Runs a `users controls` command.
///
/// # Errors
///
/// Returns an error for an unknown user or control, or a failed read or
/// write.
async fn run_controls(
    store: &bookclerk_library::LibraryStore,
    command: ControlsCommand,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        ControlsCommand::Get { user, default } => match (user, default) {
            (_, true) => print_default_controls(format, &default_member_controls(store).await?),
            (Some(user), false) => {
                let user = resolve_user(store, &user).await?;
                let assigned = assigned_member_controls(store, &user).await?;
                print_user_controls(format, &user, &assigned)
            }
            (None, false) => anyhow::bail!("name a user or pass --default"),
        },
        ControlsCommand::Set {
            user,
            default,
            allow,
            deny,
            follow_default,
        } => {
            let mut changes = Vec::new();
            for (names, value) in [(&allow, Some(true)), (&deny, Some(false))] {
                for name in names {
                    changes.push((parse_control(name)?, value));
                }
            }
            for name in &follow_default {
                if name.trim() == "all" {
                    changes.extend(
                        MemberControl::ALL
                            .into_iter()
                            .map(|control| (control, None)),
                    );
                } else {
                    changes.push((parse_control(name)?, None));
                }
            }
            if changes.is_empty() {
                anyhow::bail!("name a control with --allow, --deny or --follow-default");
            }
            match (user, default) {
                (_, true) => {
                    let mut controls = default_member_controls(store).await?;
                    for (control, value) in changes {
                        controls.set(control, value.unwrap_or(true));
                    }
                    let actor = ConfigActor::Operator { id: "cli".into() };
                    let stored = set_default_member_controls(store, &actor, controls).await?;
                    print_default_controls(format, &stored.body)
                }
                (Some(user), false) => {
                    let user = resolve_user(store, &user).await?;
                    let mut overrides = member_control_overrides(store, user.id).await?;
                    for (control, value) in changes {
                        overrides.set(control, value);
                    }
                    let assigned = set_member_control_overrides(store, user.id, overrides).await?;
                    print_user_controls(format, &user, &assigned)
                }
                (None, false) => anyhow::bail!("name a user or pass --default"),
            }
        }
    }
}

/// Parses a control name.
fn parse_control(name: &str) -> anyhow::Result<MemberControl> {
    MemberControl::parse(name.trim()).ok_or_else(|| {
        let known: Vec<&str> = MemberControl::ALL.iter().map(|c| c.as_str()).collect();
        anyhow::anyhow!("unknown control `{name}`; use one of {}", known.join(", "))
    })
}

/// The text form of one control's value.
fn allowed_word(value: bool) -> &'static str {
    if value {
        "allowed"
    } else {
        "off"
    }
}

/// Prints the library's default controls.
fn print_default_controls(format: OutputFormat, controls: &MemberControls) -> anyhow::Result<()> {
    emit(format, &json!({ "controls": controls }), || {
        println!("Default member controls");
        for control in MemberControl::ALL {
            println!(
                "  {:<14} {}",
                control.as_str(),
                allowed_word(controls.allows(control))
            );
        }
    })
}

/// Prints a patron's controls.
fn print_user_controls(
    format: OutputFormat,
    user: &UserRecord,
    assigned: &AssignedControls,
) -> anyhow::Result<()> {
    let applies = role_has_controls(user.role);
    let follows_default = role_follows_default_controls(user.role);
    emit(
        format,
        &json!({
            "user_id": user.id,
            "defaults": assigned.defaults,
            "overrides": assigned.overrides,
            "effective": assigned.effective,
            "applies": applies,
            "follows_default": follows_default,
        }),
        || {
            println!("User {}", user.id);
            if !applies {
                println!(
                    "  not applied: a {} may always do everything",
                    user.role.as_str()
                );
            }
            for control in MemberControl::ALL {
                let source = if assigned.overrides.get(control).is_some() {
                    "set for this user"
                } else if follows_default {
                    "library default"
                } else {
                    "not set"
                };
                println!(
                    "  {:<14} {} ({source})",
                    control.as_str(),
                    allowed_word(assigned.effective.allows(control))
                );
            }
        },
    )
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
    fn controls_parse_by_wire_name() {
        assert_eq!(
            parse_control(" see_prices ").unwrap(),
            MemberControl::SeePrices
        );
        assert!(parse_control("fly").is_err());
        let mut controls = MemberControls::all(true);
        controls.set(MemberControl::Upload, false);
        assert!(!controls.upload && controls.download);
    }

    /// A `users controls set` with the given changes.
    fn set(
        user: Option<&str>,
        allow: &[&str],
        deny: &[&str],
        follow_default: &[&str],
    ) -> ControlsCommand {
        let owned = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect();
        ControlsCommand::Set {
            default: user.is_none(),
            user: user.map(str::to_string),
            allow: owned(allow),
            deny: owned(deny),
            follow_default: owned(follow_default),
        }
    }

    #[tokio::test]
    async fn controls_commands_read_and_change_defaults_and_overrides() {
        use bookclerk_library::{LibraryStore, UserRole};
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        let member = store
            .create_user_with_login(UserRole::Member, Some("Kit"), Some("kit"), None)
            .await
            .unwrap();
        let owner = store
            .create_user_with_login(UserRole::Owner, Some("Ola"), Some("ola"), None)
            .await
            .unwrap();

        for format in [OutputFormat::Text, OutputFormat::Json] {
            run_controls(
                &store,
                ControlsCommand::Get {
                    user: None,
                    default: true,
                },
                format,
            )
            .await
            .unwrap();
            for user in ["kit", "ola"] {
                let get = ControlsCommand::Get {
                    user: Some(user.into()),
                    default: false,
                };
                run_controls(&store, get, format).await.unwrap();
            }
        }

        // The default turns uploads off; the member follows it.
        run_controls(&store, set(None, &[], &["upload"], &[]), OutputFormat::Text)
            .await
            .unwrap();
        assert!(!default_member_controls(&store).await.unwrap().upload);
        let assigned = assigned_member_controls(&store, &member).await.unwrap();
        assert!(!assigned.effective.upload);

        // An override allows it for the member alone, and shows in the text.
        run_controls(
            &store,
            set(Some("kit"), &["upload"], &["see_prices"], &[]),
            OutputFormat::Text,
        )
        .await
        .unwrap();
        let assigned = assigned_member_controls(&store, &member).await.unwrap();
        assert!(assigned.effective.upload && !assigned.effective.see_prices);

        // `all` drops every override again.
        run_controls(
            &store,
            set(Some("kit"), &[], &[], &["all"]),
            OutputFormat::Json,
        )
        .await
        .unwrap();
        let overrides = member_control_overrides(&store, member.id).await.unwrap();
        assert!(MemberControl::ALL
            .iter()
            .all(|c| overrides.get(*c).is_none()));
        run_controls(
            &store,
            set(Some("kit"), &[], &[], &["download"]),
            OutputFormat::Json,
        )
        .await
        .unwrap();
        run_controls(
            &store,
            ControlsCommand::Get {
                user: Some(owner.id.to_string()),
                default: false,
            },
            OutputFormat::Text,
        )
        .await
        .unwrap();

        // Mistakes are refused.
        let err = run_controls(&store, set(Some("kit"), &[], &[], &[]), OutputFormat::Text)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("name a control"), "{err}");
        let err = run_controls(
            &store,
            set(Some("kit"), &["fly"], &[], &[]),
            OutputFormat::Text,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown control"), "{err}");
        assert!(run_controls(
            &store,
            set(Some("nobody"), &["upload"], &[], &[]),
            OutputFormat::Text
        )
        .await
        .is_err());
        let unnamed = ControlsCommand::Get {
            user: None,
            default: false,
        };
        assert!(run_controls(&store, unnamed, OutputFormat::Text)
            .await
            .is_err());
        let unnamed = ControlsCommand::Set {
            user: None,
            default: false,
            allow: vec!["upload".into()],
            deny: Vec::new(),
            follow_default: Vec::new(),
        };
        assert!(run_controls(&store, unnamed, OutputFormat::Text)
            .await
            .is_err());
        assert_eq!(allowed_word(true), "allowed");
        assert_eq!(allowed_word(false), "off");
    }

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
