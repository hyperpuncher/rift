use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rift::api::{self, Request};
use rift::config;
use rift::storage::{Limits, Store};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(version, about = "Multi-format Wayland clipboard manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the clipboard daemon.
    Daemon {
        /// Capture history without automatically taking clipboard ownership.
        #[arg(long)]
        observe_only: bool,
    },
    /// List history items from newest to oldest.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a history item's manifest.
    Show { id: String },
    /// Write one MIME payload to standard output.
    Read {
        id: String,
        #[arg(long)]
        mime: String,
    },
    /// Make a history item the current clipboard selection.
    Use { id: String },
    /// Remove an item from history.
    Delete { id: String },
    /// Remove all history items.
    Clear,
    /// Show daemon and storage settings.
    Status,
}

#[derive(Debug, Serialize)]
struct Status {
    config_file: String,
    state_directory: String,
    stored_items: usize,
    stored_bytes: u64,
    maximum_items: usize,
    maximum_item_bytes: u64,
    maximum_history_bytes: u64,
    mime_inactivity_timeout_seconds: u64,
    mime_retries: usize,
    sensitive_activation_timeout_seconds: u64,
    daemon_running: bool,
    assume_ownership: Option<bool>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error);
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let (settings, config_file) = config::Config::load_or_create()?;
    let state_dir = config::state_dir()?;
    let store = Store::with_limits(state_dir.clone(), Limits::from(&settings))?;

    match cli.command {
        Command::List { json } => {
            if json {
                serde_json::to_writer_pretty(std::io::stdout(), &store.list_items()?)?;
                println!();
            } else {
                for item in store.load_index()?.items {
                    println!(
                        "{}\t{} formats\t{} bytes{}{}",
                        &item.id[..12],
                        item.format_count,
                        item.stored_bytes,
                        if item.complete { "" } else { "\tincomplete" },
                        if item.sensitive { "\tsensitive" } else { "" },
                    );
                }
            }
        }
        Command::Show { id } => {
            let manifest = store.manifest(&store.resolve_id(&id)?)?;
            serde_json::to_writer_pretty(std::io::stdout(), &manifest)?;
            println!();
        }
        Command::Read { id, mime } => {
            let id = store.resolve_id(&id)?;
            let mut payload = store.payload(&id, &mime)?;
            std::io::copy(&mut payload, &mut std::io::stdout().lock())?;
        }
        Command::Delete { id } => {
            if daemon_running() {
                daemon_request(&Request::Delete { id })?;
            } else {
                let id = store.resolve_id(&id)?;
                store.delete(&id)?;
            }
        }
        Command::Clear => {
            if daemon_running() {
                daemon_request(&Request::Clear)?;
            } else {
                store.clear()?;
            }
        }
        Command::Daemon { observe_only } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(rift::wayland::observe(&store, &settings, !observe_only))?;
        }
        Command::Status => {
            let index = store.load_index()?;
            let daemon_status = daemon_status();
            let effective_settings = daemon_status
                .as_ref()
                .and_then(|data| data.get("config"))
                .and_then(|config| serde_json::from_value(config.clone()).ok())
                .unwrap_or_else(|| settings.clone());
            let status = Status {
                config_file: config_file.display().to_string(),
                state_directory: state_dir.display().to_string(),
                stored_items: index.items.len(),
                stored_bytes: index.items.iter().map(|item| item.stored_bytes).sum(),
                maximum_items: effective_settings.max_items,
                maximum_item_bytes: effective_settings.max_item_bytes(),
                maximum_history_bytes: effective_settings.max_history_bytes(),
                mime_inactivity_timeout_seconds: effective_settings.stream_timeout_seconds,
                mime_retries: effective_settings.mime_retries,
                sensitive_activation_timeout_seconds: effective_settings.sensitive_timeout_seconds,
                daemon_running: daemon_status.is_some(),
                assume_ownership: daemon_status
                    .as_ref()
                    .and_then(|data| data.get("assume_ownership")?.as_bool()),
            };
            serde_json::to_writer_pretty(std::io::stdout(), &status)?;
            println!();
        }
        Command::Use { id } => {
            daemon_request(&Request::Use { id })?;
        }
    }

    Ok(())
}

fn daemon_running() -> bool {
    config::socket_path().is_ok_and(|path| path.exists())
}

fn daemon_status() -> Option<serde_json::Value> {
    if daemon_running() {
        daemon_request(&Request::Status).ok()
    } else {
        None
    }
}

fn daemon_request(request: &Request) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let response = api::request(request)?;
    if response.ok {
        Ok(response.data.unwrap_or(serde_json::Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "daemon request failed".to_owned())
            .into())
    }
}
