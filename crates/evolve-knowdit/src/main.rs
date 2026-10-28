mod cli;
mod pipeline;

use std::io::IsTerminal;

use clap::Parser;
use color_eyre::eyre::Result;
use evolve_knowdit_types::RunLayout;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::cli::{Command, ProgramCommand};
use crate::pipeline::{InspectCommand, PreflightCommand, ReportCommand, RunPipeline};

impl ProgramCommand {
    /// The output directory of the invoked command, where the run log goes.
    fn log_dir(&self) -> Option<std::path::PathBuf> {
        match &self.command {
            Command::Run(a) => Some(RunLayout::new(&a.out).logs_dir),
            Command::Report(a) => Some(RunLayout::new(&a.out).logs_dir),
            Command::Preflight(a) => Some(RunLayout::new(&a.out).logs_dir),
            Command::Inspect(_) => None,
        }
    }

    async fn run(self) -> Result<()> {
        match self.command {
            Command::Run(args) => RunPipeline { args: *args }.run().await,
            Command::Inspect(args) => InspectCommand::run(args).await,
            Command::Report(args) => ReportCommand::run(args).await,
            Command::Preflight(args) => PreflightCommand::run(args).await,
        }
    }
}

/// Installs the env-filtered stderr subscriber, plus a plain-text copy of the log inside the
/// command's output directory when it has one. Returns the appender guard that flushes the
/// file on exit.
fn init_tracing(
    use_colors: bool,
    log_dir: Option<std::path::PathBuf>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env()
        .expect("env contains non-utf8");
    let stderr = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(use_colors)
        .with_target(false);
    match log_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir).expect("can not create the log directory");
            let name = format!("run-{}.log", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
            let (writer, guard) =
                tracing_appender::non_blocking(tracing_appender::rolling::never(dir, name));
            let file = fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(stderr)
                .with(file)
                .init();
            Some(guard)
        }
        None => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(stderr)
                .init();
            None
        }
    }
}

fn main() {
    let use_colors = std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var("NO_COLOR") == Err(std::env::VarError::NotPresent);
    if use_colors {
        color_eyre::install().expect("init color_eyre");
    } else {
        color_eyre::config::HookBuilder::new()
            .theme(color_eyre::config::Theme::new())
            .install()
            .expect("init no color color_eyre");
    }
    let direnv_exists = std::env::var("DIRENV_DIR").is_ok();
    if !direnv_exists {
        if let Ok(dot_file) = std::env::var("DOT") {
            dotenvy::from_path_override(dot_file).expect("can not read dotenvy");
        } else {
            // Allows failure and do not override
            let _ = dotenvy::dotenv();
        }
    }

    let cmd = ProgramCommand::parse();
    let _guard = init_tracing(use_colors, cmd.log_dir());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can not build tokio");

    if let Err(err) = runtime.block_on(cmd.run()) {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}
