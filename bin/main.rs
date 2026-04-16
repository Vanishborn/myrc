use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use clap_mangen::Man;
use colored::control::set_override;
use shadow_rs::{formatcp, shadow};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use std::fs;
use std::path::PathBuf;
use std::process;

use myrc::common::MyrcError;

use myrc::account_running;
use myrc::account_usage;
use myrc::accounts;
use myrc::common::OutputMode;
use myrc::job_estimate;
use myrc::job_header;
use myrc::job_list;
use myrc::job_stats;
use myrc::maxwalltime;
use myrc::modules_setup;
use myrc::sstate;
use myrc::usage;

shadow!(build);

const AUTHOR: &str = "Haoran \"Henry\" Li @ University of Michigan";

const CUSTOM_VERSION: &str = formatcp!(
    "{}\nAuthor: {}\nTarget: {}",
    build::PKG_VERSION,
    AUTHOR,
    build::BUILD_TARGET,
);

const ABOUT: &str = formatcp!(
    "Unified CLI for UM HPC cluster resources\nAuthor: {}",
    AUTHOR,
);

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColorMode {
    /// Automatic: color when stdout is a terminal.
    Auto,
    /// Always emit color, even through a pipe.
    Always,
    /// Never emit color.
    Never,
}

#[derive(Parser)]
#[command(name = "myrc", about = ABOUT)]
#[command(version = CUSTOM_VERSION)]
struct Cli {
    /// Output as JSON instead of table.
    #[arg(long, global = true)]
    json: bool,

    /// Control color output (auto, always, never).
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true, hide_possible_values = true)]
    color: ColorMode,

    /// Increase verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List Slurm resource accounts and their limits.
    Accounts(accounts::Args),
    /// Account-scoped subcommands.
    #[command(subcommand)]
    Account(AccountCommands),
    /// Job-scoped subcommands.
    #[command(subcommand)]
    Job(JobCommands),
    /// Calculate max walltime before next maintenance window.
    Maxwalltime(maxwalltime::Args),
    /// Module-scoped subcommands.
    #[command(subcommand)]
    Modules(ModulesCommands),
    /// Per-node cluster resource dashboard.
    Sstate(sstate::Args),
    /// Show billing usage (as dollars) across accounts for a month.
    Usage(usage::Args),
    /// Generate shell completion script.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },
    /// Generate man pages to a directory.
    #[command(hide = true)]
    GenerateMan {
        /// Output directory for man pages.
        dir: PathBuf,
    },
    /// Generate completion scripts to a directory.
    #[command(hide = true)]
    GenerateCompletions {
        /// Output directory for completion scripts.
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum AccountCommands {
    /// Report monthly cost data per-user for an account.
    Usage(account_usage::Args),
    /// Show cumulative resources of running jobs for an account.
    Running(account_running::Args),
}

#[derive(Subcommand)]
enum JobCommands {
    /// Estimate the dollar cost of a hypothetical job.
    Estimate(job_estimate::Args),
    /// Print Slurm job environment header.
    Header(job_header::Args),
    /// List and filter user's jobs.
    List(job_list::Args),
    /// Show detailed job statistics and efficiency.
    Stats(job_stats::Args),
}

#[derive(Subcommand)]
enum ModulesCommands {
    /// Set up personal Lmod module directory.
    Setup(modules_setup::Args),
}

fn main() {
    // Reset SIGPIPE to default so piping into head/tail exits cleanly
    // instead of panicking. Rust ignores SIGPIPE by default.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    human_panic::setup_panic!();

    if let Err(err) = run() {
        let code = err
            .downcast_ref::<MyrcError>()
            .map(MyrcError::exit_code)
            .unwrap_or(myrc::common::ExitCode::Failure);
        eprintln!("error: {err:#}");
        process::exit(code.code());
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Apply color mode before any output
    match cli.color {
        ColorMode::Always => set_override(true),
        ColorMode::Never => set_override(false),
        ColorMode::Auto => {} // let colored crate decide (TTY + NO_COLOR)
    }

    let log_level = match cli.verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    let filter =
        EnvFilter::try_from_env("MYRC_LOG").unwrap_or_else(|_| EnvFilter::new(log_level.as_str()));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();

    let mode = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Table
    };

    match cli.command {
        Commands::Accounts(ref args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(accounts::run(args, mode))
        }
        Commands::Account(ref sub) => {
            let rt = tokio::runtime::Runtime::new()?;
            match sub {
                AccountCommands::Usage(args) => rt.block_on(account_usage::run(args, mode)),
                AccountCommands::Running(args) => rt.block_on(account_running::run(args, mode)),
            }
        }
        Commands::Job(ref sub) => {
            let rt = tokio::runtime::Runtime::new()?;
            match sub {
                JobCommands::Estimate(args) => rt.block_on(job_estimate::run(args, mode)),
                JobCommands::Header(args) => job_header::run(args, mode),
                JobCommands::List(args) => rt.block_on(job_list::run(args, mode)),
                JobCommands::Stats(args) => rt.block_on(job_stats::run(args, mode)),
            }
        }
        Commands::Maxwalltime(ref args) => maxwalltime::run(args, mode),
        Commands::Modules(ref sub) => match sub {
            ModulesCommands::Setup(args) => modules_setup::run(args, mode),
        },
        Commands::Sstate(ref args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(sstate::run(args, mode))
        }
        Commands::Usage(ref args) => usage::run(args, mode),
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "myrc", &mut std::io::stdout());
            Ok(())
        }
        Commands::GenerateMan { dir } => {
            fs::create_dir_all(&dir)?;
            generate_man_pages(Cli::command(), &dir, "")?;
            Ok(())
        }
        Commands::GenerateCompletions { dir } => {
            fs::create_dir_all(&dir)?;
            for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
                let filename = match shell {
                    Shell::Bash => "myrc",
                    Shell::Zsh => "_myrc",
                    Shell::Fish => "myrc.fish",
                    _ => continue,
                };
                let path = dir.join(filename);
                let mut file = fs::File::create(&path)?;
                clap_complete::generate(shell, &mut Cli::command(), "myrc", &mut file);
            }
            Ok(())
        }
    }
}

/// Recursively generate man pages for a command and all its subcommands.
fn generate_man_pages(cmd: clap::Command, dir: &std::path::Path, prefix: &str) -> Result<()> {
    let name = if prefix.is_empty() {
        cmd.get_name().to_string()
    } else {
        format!("{prefix}-{}", cmd.get_name())
    };
    let man = Man::new(cmd.clone())
        .title(name.to_uppercase())
        .section("1");
    let mut buf = Vec::new();
    man.render(&mut buf)?;
    fs::write(dir.join(format!("{name}.1")), buf)?;

    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        generate_man_pages(sub.clone(), dir, &name)?;
    }
    Ok(())
}
