//! Minimal hand-rolled CLI subcommand parsing for `lied-server`.
//!
//! Kept dependency-free (no `clap`) since there's exactly one subcommand
//! today (`create-admin`); the server's normal "just start and listen"
//! behavior remains the default when no subcommand is given. Most logic
//! lives in `lied::auth::admin::create_admin` so it's testable without
//! spawning a process — this module is just argv parsing + stdin/stdout.

use std::io::Write;

use anyhow::Context;

pub enum Command {
    CreateAdmin {
        username: String,
        email: Option<String>,
        display_name: String,
        password: Option<String>,
    },
}

/// Parse `argv` (excluding the program name) into a [`Command`], or `None`
/// if no recognized subcommand was given (the caller should fall through to
/// the normal server-start path).
pub fn parse_args(args: impl Iterator<Item = String>) -> Option<Command> {
    let mut args: Vec<String> = args.collect();
    if args.first().map(String::as_str) != Some("create-admin") {
        return None;
    }
    args.remove(0);

    let mut username = None;
    let mut email = None;
    let mut display_name = None;
    let mut password = None;

    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--username" => username = iter.next(),
            "--email" => email = iter.next(),
            "--display-name" => display_name = iter.next(),
            "--password" => password = iter.next(),
            other => {
                eprintln!("warning: ignoring unrecognized argument '{other}'");
            }
        }
    }

    Some(Command::CreateAdmin {
        username: username.unwrap_or_default(),
        email,
        display_name: display_name.unwrap_or_default(),
        password,
    })
}

/// Execute a parsed [`Command`]. Connects to the DB using the same
/// `AppConfig`/`AppState` path the server itself uses (so it honors the
/// same `LIED_DATABASE_URL` / secrets resolution), runs the operation, and
/// prints a confirmation to stdout.
pub async fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::CreateAdmin {
            username,
            email,
            display_name,
            password,
        } => create_admin(username, email, display_name, password).await,
    }
}

async fn create_admin(
    username: String,
    email: Option<String>,
    display_name: String,
    password_arg: Option<String>,
) -> anyhow::Result<()> {
    if username.is_empty() {
        anyhow::bail!("--username is required");
    }
    if display_name.is_empty() {
        anyhow::bail!("--display-name is required");
    }

    let password = match password_arg {
        Some(p) => p,
        None => read_password_from_stdin()?,
    };
    if password.is_empty() {
        anyhow::bail!("password must not be empty");
    }

    let config = lied::config::AppConfig::load().context("failed to load configuration")?;
    let state = lied::state::AppState::connect(config)
        .await
        .context("failed to connect to the database")?;

    if lied::domain::user::any_system_admin_exists(&state.db)
        .await
        .context("failed to check for existing system admins")?
    {
        println!("note: at least one system admin already exists; creating another.");
    }

    let created = lied::auth::admin::create_admin(
        &state.db,
        lied::auth::admin::CreateAdminParams {
            username: &username,
            email: email.as_deref(),
            display_name: &display_name,
            password: &password,
        },
    )
    .await
    .context("failed to create admin user")?;

    println!(
        "Created system admin '{}' (id={}, slug={}).",
        created.username, created.id, created.slug
    );

    Ok(())
}

/// Read a password from stdin (a single line, trimmed of the trailing
/// newline). No TTY-echo suppression (would need `rpassword`); acceptable
/// for a bootstrap CLI run by an operator on a server they already control,
/// and keeps the dependency footprint minimal per the issue's guidance.
fn read_password_from_stdin() -> anyhow::Result<String> {
    print!("Admin password: ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("failed to read password from stdin")?;

    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_returns_none_for_no_subcommand() {
        assert!(parse_args(std::iter::empty()).is_none());
    }

    #[test]
    fn parse_args_returns_none_for_unrecognized_subcommand() {
        let args = vec!["serve".to_string()];
        assert!(parse_args(args.into_iter()).is_none());
    }

    #[test]
    fn parse_args_parses_create_admin_flags() {
        let args = vec![
            "create-admin".to_string(),
            "--username".to_string(),
            "alice".to_string(),
            "--email".to_string(),
            "alice@example.com".to_string(),
            "--display-name".to_string(),
            "Alice Archivist".to_string(),
            "--password".to_string(),
            "secret123".to_string(),
        ];
        let command = parse_args(args.into_iter()).expect("should parse");
        match command {
            Command::CreateAdmin {
                username,
                email,
                display_name,
                password,
            } => {
                assert_eq!(username, "alice");
                assert_eq!(email, Some("alice@example.com".to_string()));
                assert_eq!(display_name, "Alice Archivist");
                assert_eq!(password, Some("secret123".to_string()));
            }
        }
    }

    #[test]
    fn parse_args_allows_missing_optional_flags() {
        let args = vec![
            "create-admin".to_string(),
            "--username".to_string(),
            "bob".to_string(),
            "--display-name".to_string(),
            "Bob".to_string(),
        ];
        let command = parse_args(args.into_iter()).expect("should parse");
        match command {
            Command::CreateAdmin {
                email, password, ..
            } => {
                assert_eq!(email, None);
                assert_eq!(password, None);
            }
        }
    }
}
