//! Standalone migration entry point.
//!
//! Applies the embedded Postgres migrations through the same owned runner
//! the server uses (`djinn_db::migrations`), without linking the rest of
//! `djinn-server`. CI needs a migrated schema in six jobs per run; building
//! `djinn-server` for that pulls ~200 packages it never executes (kube,
//! tree-sitter grammars, candle, the coordinator/agent crates) and cost
//! ~8 minutes per job. This binary keeps the designated-operator contract
//! from `migrations.rs` intact while staying inside `djinn-db`'s own
//! dependency closure.
//!
//! Flag names deliberately mirror `djinn-server`'s so the CI invocations
//! read the same.
#![allow(clippy::print_stderr)]

use std::time::Duration;

use djinn_db::migrations::{
    DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
    ensure_postgres_database_exists, run_postgres_migrations,
};
use sqlx::Connection;
use sqlx::postgres::PgConnection;

const USAGE: &str = "\
usage: djinn-migrate [--migrate-only | --bootstrap-designated-operator-only] [options]

  --database-url <url>                        (default: $DJINN_DATABASE_URL)
  --migration-designated-operator-user-id <uuid>
  --bootstrap-designated-operator-github-id <i64>
  --bootstrap-designated-operator-github-login <login>
  --bootstrap-designated-operator-github-name <name>
";

#[derive(Default)]
struct Args {
    database_url: Option<String>,
    migrate_only: bool,
    bootstrap_only: bool,
    operator_user_id: Option<String>,
    github_id: Option<i64>,
    github_login: Option<String>,
    github_name: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--database-url" => args.database_url = Some(value()?),
            "--migrate-only" => args.migrate_only = true,
            "--bootstrap-designated-operator-only" => args.bootstrap_only = true,
            "--migration-designated-operator-user-id" => args.operator_user_id = Some(value()?),
            "--bootstrap-designated-operator-github-id" => {
                let raw = value()?;
                args.github_id = Some(
                    raw.parse()
                        .map_err(|_| format!("invalid github id `{raw}`"))?,
                );
            }
            "--bootstrap-designated-operator-github-login" => args.github_login = Some(value()?),
            "--bootstrap-designated-operator-github-name" => args.github_name = Some(value()?),
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unrecognized argument `{other}`\n\n{USAGE}")),
        }
    }
    if args.migrate_only == args.bootstrap_only {
        return Err(
            "exactly one of --migrate-only or --bootstrap-designated-operator-only is required"
                .to_owned(),
        );
    }
    if args.database_url.is_none() {
        args.database_url = std::env::var("DJINN_DATABASE_URL").ok();
    }
    if args.database_url.is_none() {
        return Err("no database url: pass --database-url or set DJINN_DATABASE_URL".to_owned());
    }
    Ok(args)
}

/// Mirror the server's startup wait so a slow Postgres service container
/// surfaces as a retry rather than an immediate connection error.
async fn wait_for_database(db_url: &str) -> Result<(), String> {
    // `tokio::time::timeout` owns the deadline so this stays clear of the
    // repo-wide `Instant::now` ban (clippy.toml disallowed-methods).
    let keep_trying = async {
        loop {
            match PgConnection::connect(db_url).await {
                Ok(conn) => {
                    let _ = conn.close().await;
                    return;
                }
                Err(e) => eprintln!("djinn-migrate: waiting for database ({e})"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(60), keep_trying)
        .await
        .map_err(|_| "database unreachable after 60s".to_owned())
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let db_url = args.database_url.as_deref().unwrap_or_default();

    if let Err(e) = wait_for_database(db_url).await {
        eprintln!("djinn-migrate: {e}");
        std::process::exit(1);
    }
    if let Err(e) = ensure_postgres_database_exists(db_url).await {
        eprintln!("djinn-migrate: failed to ensure database exists: {e}");
        std::process::exit(1);
    }

    if args.bootstrap_only {
        let (Some(user_id), Some(github_id), Some(github_login)) =
            (args.operator_user_id, args.github_id, args.github_login)
        else {
            eprintln!("djinn-migrate: bootstrap requires operator id, GitHub id, and GitHub login");
            std::process::exit(2);
        };
        if let Err(e) = bootstrap_designated_operator(
            db_url,
            &DesignatedOperatorBootstrap {
                user_id,
                github_id,
                github_login,
                github_name: args.github_name,
                github_avatar_url: None,
            },
        )
        .await
        {
            eprintln!("djinn-migrate: designated operator bootstrap failed: {e}");
            std::process::exit(1);
        }
        eprintln!("djinn-migrate: designated operator provisioned");
        return;
    }

    if let Err(e) = run_postgres_migrations(
        db_url,
        &MigrationContext {
            designated_operator_user_id: args.operator_user_id,
        },
    )
    .await
    {
        eprintln!("djinn-migrate: schema migration failed: {e}");
        std::process::exit(1);
    }
    eprintln!("djinn-migrate: all migrations applied");
}
