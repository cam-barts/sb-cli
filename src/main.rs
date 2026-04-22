use clap::Parser;
use std::process;
use tracing::debug;

use sb_cli::cli::{
    AuthCommands, Cli, Commands, ConfigCommands, PageCommands, ServerCommands, SyncCommands,
};
use sb_cli::commands;
use sb_cli::output::{self, OutputConfig};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize tracing-subscriber for logging
    output::init_tracing(cli.verbose);

    // Build output configuration from CLI flags + env
    let output_config = OutputConfig::new(cli.quiet, cli.verbose, cli.no_color);

    // Apply color settings globally via console crate
    if !output_config.color {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    debug!(
        verbose = cli.verbose,
        quiet = cli.quiet,
        color = output_config.color,
        "output configuration resolved"
    );

    let result = match cli.command {
        Some(Commands::Version) => {
            debug!("dispatching: version");
            commands::version::execute(cli.quiet, output_config.color)
        }
        Some(Commands::Config { command }) => {
            debug!("dispatching: config");
            match command {
                ConfigCommands::Show { reveal } => {
                    commands::config::execute_show(reveal, &cli.format, cli.quiet)
                }
                ConfigCommands::SetSpace { path } => {
                    commands::config::execute_set_space(&path, cli.quiet, output_config.color)
                }
                ConfigCommands::GetSpace => {
                    commands::config::execute_get_space(&cli.format, cli.quiet)
                }
            }
        }
        Some(Commands::Init { server_url }) => {
            debug!("dispatching: init");
            commands::init::execute(
                server_url,
                cli.token.clone(),
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Server { command }) => {
            debug!("dispatching: server");
            match command {
                ServerCommands::Ping => {
                    commands::server::execute_ping(
                        cli.token.as_deref(),
                        &cli.format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                ServerCommands::Config => {
                    commands::server::execute_config(
                        cli.token.as_deref(),
                        &cli.format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
            }
        }
        Some(Commands::Auth { command }) => {
            debug!("dispatching: auth");
            match command {
                AuthCommands::Set { token } => {
                    commands::auth::execute_set(token, cli.quiet, output_config.color).await
                }
            }
        }
        Some(Commands::Page { command }) => {
            debug!("dispatching: page");
            match command {
                PageCommands::List { sort, limit } => {
                    commands::page::execute_list(
                        &sort,
                        limit,
                        &cli.format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                PageCommands::Read { name, remote } => {
                    commands::page::execute_read(
                        cli.token.as_deref(),
                        &name,
                        remote,
                        &cli.format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                PageCommands::Create {
                    name,
                    content,
                    edit,
                    template,
                } => {
                    commands::page::execute_create(
                        cli.token.as_deref(),
                        &name,
                        content.as_deref(),
                        edit,
                        template.as_deref(),
                        &cli.format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                PageCommands::Edit { name } => {
                    commands::page::execute_edit(&name, cli.quiet, output_config.color).await
                }
                PageCommands::Delete { name, force } => {
                    commands::page::execute_delete(&name, force, cli.quiet, output_config.color)
                        .await
                }
                PageCommands::Append { name, content } => {
                    commands::page::execute_append(&name, &content, cli.quiet, output_config.color)
                        .await
                }
                PageCommands::Move { name, new_name } => {
                    commands::page::execute_move(&name, &new_name, cli.quiet, output_config.color)
                        .await
                }
            }
        }
        Some(Commands::Daily {
            append,
            yesterday,
            offset,
        }) => {
            debug!("dispatching: daily");
            commands::daily::execute(
                cli.token.as_deref(),
                append.as_deref(),
                yesterday,
                offset,
                &cli.format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Sync { command, dry_run }) => {
            debug!("dispatching: sync");
            match command {
                Some(SyncCommands::Pull {
                    dry_run: sub_dry_run,
                }) => {
                    commands::sync::execute_pull(
                        cli.token.as_deref(),
                        cli.quiet,
                        &cli.format,
                        dry_run || sub_dry_run,
                    )
                    .await
                }
                Some(SyncCommands::Push {
                    dry_run: sub_dry_run,
                }) => {
                    commands::sync::execute_push(
                        cli.token.as_deref(),
                        cli.quiet,
                        &cli.format,
                        dry_run || sub_dry_run,
                    )
                    .await
                }
                Some(SyncCommands::Status) => commands::sync::execute_status(&cli.format).await,
                Some(SyncCommands::Conflicts) => {
                    commands::sync::execute_conflicts(&cli.format).await
                }
                Some(SyncCommands::Resolve {
                    path,
                    keep_local,
                    keep_remote,
                    diff,
                    force,
                }) => {
                    commands::sync::execute_resolve(
                        cli.token.as_deref(),
                        &path,
                        keep_local,
                        keep_remote,
                        diff,
                        force,
                        cli.quiet,
                        &cli.format,
                    )
                    .await
                }
                None => {
                    if dry_run {
                        commands::sync::execute_sync_dry_run(
                            cli.token.as_deref(),
                            cli.quiet,
                            &cli.format,
                        )
                        .await
                    } else {
                        commands::sync::execute_sync(cli.token.as_deref(), cli.quiet, &cli.format)
                            .await
                    }
                }
            }
        }
        Some(Commands::Lua { expression }) => {
            debug!("dispatching: lua");
            commands::lua::execute(
                cli.token.as_deref(),
                &expression,
                &cli.format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Query { query }) => {
            debug!("dispatching: query");
            commands::query::execute(
                cli.token.as_deref(),
                &query,
                &cli.format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Shell { command }) => {
            debug!("dispatching: shell");
            commands::shell::execute(
                cli.token.as_deref(),
                &command,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        None => {
            // No subcommand: print help
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            cmd.print_help().ok();
            println!();
            return;
        }
    };

    if let Err(e) = result {
        output::print_error(&e, output_config.color);
        process::exit(e.exit_code());
    }
}
