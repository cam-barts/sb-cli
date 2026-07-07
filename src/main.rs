use clap::Parser;
use std::process;
use tracing::debug;

use sb_cli::cli::{
    AuthCommands, Cli, Commands, ConfigCommands, PageCommands, ServerCommands, SyncCommands,
    TemplateCommands,
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

    // Resolve --format once: explicit user value wins, otherwise auto-detect
    // (Human when stdout is a TTY, Json when piped).
    let format = output::resolve_format(cli.format.clone());

    // Apply color settings globally via console crate
    if !output_config.color {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    // Record interaction flags process-globally so pickers/editor/confirmations
    // can honor them without every command threading the flags through.
    output::set_no_input(cli.no_input);
    output::set_assume_yes(cli.yes);

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
                    commands::config::execute_show(reveal, &format, cli.quiet)
                }
                ConfigCommands::SetSpace { path } => {
                    commands::config::execute_set_space(&path, cli.quiet, output_config.color)
                }
                ConfigCommands::GetSpace => commands::config::execute_get_space(&format, cli.quiet),
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
                        &format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                ServerCommands::Config => {
                    commands::server::execute_config(
                        cli.token.as_deref(),
                        &format,
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
                PageCommands::List {
                    sort,
                    limit,
                    fields,
                } => {
                    commands::page::execute_list(
                        &sort,
                        limit,
                        &fields,
                        &format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                PageCommands::Read { name, remote } => {
                    commands::page::execute_read(
                        cli.token.as_deref(),
                        name.as_deref(),
                        remote,
                        &format,
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
                    upsert,
                } => {
                    commands::page::execute_create(
                        cli.token.as_deref(),
                        &name,
                        content.as_deref(),
                        edit,
                        template.as_deref(),
                        upsert,
                        &format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                PageCommands::Edit { name } => {
                    commands::page::execute_edit(name.as_deref(), cli.quiet, output_config.color)
                        .await
                }
                PageCommands::Delete {
                    name,
                    force,
                    dry_run,
                } => {
                    commands::page::execute_delete(
                        name.as_deref(),
                        force,
                        dry_run,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                PageCommands::Append { name, content } => {
                    commands::page::execute_append(&name, &content, cli.quiet, output_config.color)
                        .await
                }
                PageCommands::Move {
                    name,
                    new_name,
                    dry_run,
                } => {
                    commands::page::execute_move(
                        &name,
                        &new_name,
                        dry_run,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
            }
        }
        Some(Commands::Daily {
            entry,
            yesterday,
            offset,
            on,
            star,
            time,
            no_time,
            task,
            task_tag,
            no_task_tag,
            append,
            limit,
            from,
            to,
            contains,
            tags,
            starred,
            short,
        }) => {
            debug!("dispatching: daily");
            commands::daily::execute(commands::daily::DailyArgs {
                cli_token: cli.token.as_deref(),
                entry,
                yesterday,
                offset,
                on: on.as_deref(),
                star,
                time: time.as_deref(),
                no_time,
                task,
                task_tag: task_tag.as_deref(),
                no_task_tag,
                append: append.as_deref(),
                limit,
                from: from.as_deref(),
                to: to.as_deref(),
                contains: contains.as_deref(),
                tags,
                starred,
                short,
                format: &format,
                quiet: cli.quiet,
                color: output_config.color,
            })
            .await
        }
        Some(Commands::Sync {
            command,
            dry_run,
            workers,
        }) => {
            debug!("dispatching: sync");
            match command {
                Some(SyncCommands::Pull {
                    dry_run: sub_dry_run,
                    workers: sub_workers,
                }) => {
                    commands::sync::execute_pull(
                        cli.token.as_deref(),
                        cli.quiet,
                        &format,
                        dry_run || sub_dry_run,
                        sub_workers.or(workers),
                    )
                    .await
                }
                Some(SyncCommands::Push {
                    dry_run: sub_dry_run,
                    workers: sub_workers,
                }) => {
                    commands::sync::execute_push(
                        cli.token.as_deref(),
                        cli.quiet,
                        &format,
                        dry_run || sub_dry_run,
                        sub_workers.or(workers),
                    )
                    .await
                }
                Some(SyncCommands::Status) => commands::sync::execute_status(&format).await,
                Some(SyncCommands::Conflicts) => commands::sync::execute_conflicts(&format).await,
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
                        &format,
                    )
                    .await
                }
                None => {
                    if dry_run {
                        commands::sync::execute_sync_dry_run(
                            cli.token.as_deref(),
                            cli.quiet,
                            &format,
                        )
                        .await
                    } else {
                        commands::sync::execute_sync(
                            cli.token.as_deref(),
                            cli.quiet,
                            &format,
                            workers,
                        )
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
                &format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Query { query, fields }) => {
            debug!("dispatching: query");
            commands::query::execute(
                cli.token.as_deref(),
                &query,
                &fields,
                &format,
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
        Some(Commands::Logs {
            follow,
            interval_ms,
            source,
        }) => {
            debug!("dispatching: logs");
            commands::logs::execute(
                cli.token.as_deref(),
                follow,
                interval_ms,
                source.into(),
                &format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Screenshot { output: out_path }) => {
            debug!("dispatching: screenshot");
            commands::screenshot::execute(
                cli.token.as_deref(),
                out_path.as_deref(),
                &format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Describe {
            tag,
            limit,
            out_fields,
        }) => {
            debug!("dispatching: describe");
            commands::describe::execute(
                cli.token.as_deref(),
                &tag,
                limit,
                &out_fields,
                &format,
                cli.quiet,
                output_config.color,
            )
            .await
        }
        Some(Commands::Template { command }) => {
            debug!("dispatching: template");
            match command {
                TemplateCommands::List => {
                    commands::template::execute_list(
                        cli.token.as_deref(),
                        &format,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
                TemplateCommands::New {
                    name,
                    template,
                    no_edit,
                    dry_run,
                } => {
                    commands::template::execute_new(
                        cli.token.as_deref(),
                        name.as_deref(),
                        template.as_deref(),
                        no_edit,
                        dry_run,
                        cli.quiet,
                        output_config.color,
                    )
                    .await
                }
            }
        }
        Some(Commands::Completions { shell, install }) => {
            debug!("dispatching: completions");
            commands::completions::execute(shell, install, cli.quiet, output_config.color)
        }
        Some(Commands::Upgrade { check }) => {
            debug!("dispatching: upgrade");
            // self_update uses a blocking HTTP client — run it off the async
            // runtime to avoid a nested-runtime panic.
            let quiet = cli.quiet;
            let color = output_config.color;
            match tokio::task::spawn_blocking(move || {
                commands::upgrade::execute(check, quiet, color)
            })
            .await
            {
                Ok(r) => r,
                Err(e) => Err(sb_cli::error::SbError::Internal {
                    message: format!("upgrade task failed: {e}"),
                }),
            }
        }
        #[cfg(feature = "skills")]
        Some(Commands::Schema) => {
            debug!("dispatching: schema");
            commands::schema::execute(cli.quiet, output_config.color)
        }
        #[cfg(feature = "skills")]
        Some(Commands::Skills { command }) => {
            debug!("dispatching: skills");
            match command {
                sb_cli::cli::SkillsCommands::Init { target } => {
                    commands::skills::execute_init(target, cli.quiet, output_config.color)
                }
            }
        }
        #[cfg(feature = "mcp")]
        Some(Commands::Mcp { command }) => {
            debug!("dispatching: mcp");
            match command {
                sb_cli::cli::McpCommands::Serve { http } => {
                    commands::mcp::execute_serve(http, cli.quiet, output_config.color).await
                }
            }
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
        output::print_error(&e, output_config.color, &format);
        process::exit(e.exit_code());
    }
}
