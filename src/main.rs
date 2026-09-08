use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(clap::Parser)]
#[command(
    name = "minidock",
    about = "A small, root-required Linux container runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum Command {
    Run(RunArgs),
    Ps,
    Stop {
        id: String,
    },
    Logs {
        id: String,
    },
    Build {
        #[arg(long)]
        context: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    #[command(hide = true)]
    InitContainer(InitArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct RunArgs {
    #[arg(long)]
    pub image: PathBuf,
    #[arg(long)]
    pub memory: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub cpu_percent: Option<u8>,
    #[arg(long)]
    pub hostname: Option<String>,
    #[arg(short = 'd', long)]
    pub detach: bool,
    #[arg(last = true, required = true, value_name = "command")]
    pub command: Vec<OsString>,
}

use minidock::InitArgs;

fn run_request(args: RunArgs) -> anyhow::Result<minidock::RunRequest> {
    let mut command = Vec::new();
    for token in args.command {
        let s = token
            .into_string()
            .map_err(|_| anyhow::anyhow!("command contains invalid UTF-8"))?;
        command.push(s);
    }
    let hostname = match args.hostname {
        Some(h) => {
            anyhow::ensure!(h.len() <= 63, "hostname cannot exceed 63 bytes");
            h
        }
        None => "minidock".to_string(),
    };
    let memory_bytes = match args.memory {
        Some(m) => Some(minidock::parse_memory_limit(&m)?),
        None => None,
    };
    Ok(minidock::RunRequest {
        image: args.image,
        memory_bytes,
        cpu_percent: args.cpu_percent,
        hostname,
        detached: args.detach,
        command,
    })
}

fn parse_id(s: &str) -> anyhow::Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|e| anyhow::anyhow!("invalid container ID '{}': {}", s, e))
}

fn print_ps(store: minidock::StateStore) -> anyhow::Result<()> {
    let containers = store.list()?;
    println!(
        "{:<14} {:<24} {:<8} {:<10} STARTED",
        "CONTAINER ID", "COMMAND", "PID", "STATUS"
    );
    for c in containers {
        let full_id = c.id.to_string();
        let short_id = if full_id.len() >= 12 {
            &full_id[..12]
        } else {
            &full_id
        };
        let cmd = c.command.join(" ");
        let started = c
            .started_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string());
        println!(
            "{:<14} {:<24} {:<8} {:<10} {}",
            short_id, cmd, c.pid, c.status, started
        );
    }
    Ok(())
}

fn print_logs(store: &minidock::StateStore, id: uuid::Uuid) -> anyhow::Result<()> {
    let state = store.load(id)?;
    if !state.detached {
        anyhow::bail!("container was not started in detached mode; no log exists");
    }
    let mut log_file = store.open_log(id)?;
    let mut stdout = std::io::stdout();
    std::io::copy(&mut log_file, &mut stdout)?;
    Ok(())
}

fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Run(args) => {
            let req = run_request(args)?;
            let store = minidock::StateStore::from_current_user()?;
            let code = minidock::run(req, store)?;
            std::process::exit(code);
        }
        Command::Ps => print_ps(minidock::StateStore::from_current_user()?),
        Command::Stop { id } => {
            minidock::stop(parse_id(&id)?, &minidock::StateStore::from_current_user()?)
        }
        Command::Logs { id } => {
            print_logs(&minidock::StateStore::from_current_user()?, parse_id(&id)?)
        }
        Command::Build { context, output } => {
            minidock::image::build_image(&context, &output)?;
            println!("{}", output.display());
            Ok(())
        }
        Command::InitContainer(args) => minidock::init_container(args),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    dispatch(cli.command)
}
