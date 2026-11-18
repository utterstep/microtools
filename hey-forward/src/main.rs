use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{TimeDelta, Utc};
use clap::{Parser, Subcommand};
use hey_forward::auth::{self, StoredTokens};
use hey_forward::config::Config;
use hey_forward::forwarder::Forwarder;
use hey_forward::state::{State, StateFile};
use hey_sdk::Client;
use humantime::format_duration;
use tracing::{debug, info, warn};

/// How often a poll reads back through the whole lookback window rather than only what
/// moved since the last one.
const DEEP_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// The longest any single call to HEY may take, retries included.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(120);

/// Forwards HEY mail sent to chosen +aliases on to other addresses.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The rules file [default: $CONFIGURATION_DIRECTORY/config.toml, else
    /// ~/.config/hey-forward/config.toml]
    #[arg(long, global = true, env = "HEY_FORWARD_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,

    /// Where credentials and progress are kept [default: $STATE_DIRECTORY, else
    /// ~/.local/state/hey-forward]
    #[arg(long, global = true, env = "HEY_FORWARD_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// Log each message looked at, with the addresses it was sent to
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to HEY and save the credentials
    Login,
    /// Show which recent mail the rules would forward, without sending anything
    Check,
    /// Watch the mailbox and forward what matches, until stopped
    Run,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = runtime.block_on(dispatch(cli));
    // `login` may still be waiting on a line from stdin, which cannot be cancelled.
    runtime.shutdown_background();

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    let state_dir = cli.state_dir.map_or_else(default_state_dir, Ok)?;
    let credentials = state_dir.join("credentials.json");
    let hey = hey_sdk::Config::default().with_env();

    if matches!(cli.command, Command::Login) {
        return auth::login(&hey, &credentials).await;
    }

    let config = Config::load(&cli.config.map_or_else(default_config_path, Ok)?)?;
    let client = Client::builder(hey.clone())
        .token_provider(StoredTokens::open(credentials, &hey)?)
        .operation_timeout(OPERATION_TIMEOUT)
        .build()?;
    match cli.command {
        Command::Check => check(client, config).await,
        Command::Login | Command::Run => run(client, config, &state_dir).await,
    }
}

/// The daemon: poll, sleep, repeat, until asked to stop.
async fn run(client: Client, config: Config, state_dir: &Path) -> Result<()> {
    let mut file = StateFile::load_or_start(state_dir.join("state.json"), Utc::now())?;
    // Saved before the first poll: it pins the moment forwarding starts from, and finds an
    // unwritable state directory now rather than after the first forward.
    file.save()?;
    info!(
        "watching {} alias(es) every {}; mail from before {} is left alone",
        config.rules.len(),
        format_duration(config.poll_interval),
        file.state.baseline.format("%Y-%m-%d %H:%M UTC"),
    );

    let poll_interval = config.poll_interval;
    let mut forwarder = Forwarder::new(client, config, false);
    let mut stop = std::pin::pin!(stop_signal());
    let mut last_deep: Option<Instant> = None;
    loop {
        // A poll is never cut short by a stop: one caught mid-send could not tell whether
        // its forward went out.
        let deep = last_deep.is_none_or(|at| at.elapsed() >= DEEP_POLL_INTERVAL);
        match forwarder.poll(&mut file, Utc::now(), deep).await {
            Ok(report) => {
                if deep {
                    last_deep = Some(Instant::now());
                }
                debug!(
                    deep,
                    topics = report.topics,
                    messages = report.messages,
                    forwarded = report.forwarded.len(),
                    failed = report.failed,
                    "poll complete"
                );
            }
            Err(error) => warn!("poll cut short, will try again: {error:#}"),
        }
        tokio::select! {
            () = &mut stop => break,
            () = tokio::time::sleep(poll_interval) => {}
        }
    }
    info!("stopping");
    Ok(())
}

/// A dry run over the lookback window, for trying rules out before trusting them.
async fn check(client: Client, config: Config) -> Result<()> {
    let me = client
        .identity()
        .get()
        .await
        .context("cannot reach HEY as the signed-in account")?;
    println!("Signed in as {}", me.name.as_deref().unwrap_or("(unnamed)"));
    for rule in &config.rules {
        println!("  {} → {}", rule.alias, rule.to.join(", "));
    }

    let now = Utc::now();
    let lookback = TimeDelta::from_std(config.lookback).unwrap_or(TimeDelta::MAX);
    let window = format_duration(config.lookback);
    let mut file = StateFile::in_memory(State::starting_at(now - lookback));
    let report = Forwarder::new(client, config, true)
        .poll(&mut file, now, true)
        .await?;

    println!();
    for forward in &report.forwarded {
        println!(
            "{}  {:?}  {} → {}",
            forward.received_at.format("%Y-%m-%d %H:%M"),
            forward.subject,
            forward.aliases.join(", "),
            forward.to.join(", "),
        );
    }
    println!(
        "Looked at {} message(s) in {} thread(s) from the last {window}: {} would be forwarded.",
        report.messages,
        report.topics,
        report.forwarded.len(),
    );
    Ok(())
}

/// Resolves on SIGINT or SIGTERM. The handlers are installed on first poll and buffer a
/// signal that arrives while nobody is waiting, so one sent mid-poll is seen after it.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut interrupt), Ok(mut terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
            }
            _ => std::future::pending().await,
        }
    }
    #[cfg(not(unix))]
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let default = if verbose {
        "info,hey_forward=debug"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_env("HEY_FORWARD_LOG").unwrap_or_else(|_| default.into());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    // Under systemd the journal stamps every line itself.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

/// systemd's `ConfigurationDirectory=`, or the XDG config directory.
fn default_config_path() -> Result<PathBuf> {
    let directory = match first_path("CONFIGURATION_DIRECTORY") {
        Some(directory) => directory,
        None => xdg_dir("XDG_CONFIG_HOME", ".config")?.join("hey-forward"),
    };
    Ok(directory.join("config.toml"))
}

/// systemd's `StateDirectory=`, or the XDG state directory.
fn default_state_dir() -> Result<PathBuf> {
    match first_path("STATE_DIRECTORY") {
        Some(directory) => Ok(directory),
        None => Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("hey-forward")),
    }
}

/// The first path in a variable systemd fills with a colon-separated list.
fn first_path(variable: &str) -> Option<PathBuf> {
    let value = std::env::var_os(variable)?;
    std::env::split_paths(&value).find(|path| !path.as_os_str().is_empty())
}

fn xdg_dir(variable: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(variable).filter(|value| !value.is_empty()) {
        Some(value) => Ok(PathBuf::from(value)),
        None => Ok(std::env::home_dir()
            .context("no home directory: pass --config and --state-dir")?
            .join(fallback)),
    }
}
