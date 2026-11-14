use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use replique_client::config::{ClientConfig, DEFAULT_CONFIG, DEFAULT_STATE_DIR};
use replique_client::pair::PairOptions;
use replique_client::restic::Restic;
use replique_client::signals::Stop;
use replique_client::state::State;
use replique_client::{Error, log, pair, run, status};
use replique_proto::JobKind;

/// Runs restic on this device and reports every run to Réplique.
#[derive(Parser)]
#[command(name = "replique-client", version, about)]
struct Cli {
    /// The configuration with this device's jobs (written by the NixOS module).
    #[arg(long, global = true, env = "REPLIQUE_CONFIG", default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    /// Where pairing keeps its secrets and runs keep their spool and status
    /// [default: the configuration's state_dir, else /var/lib/replique-client].
    #[arg(long, global = true, env = "REPLIQUE_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair this device with the code shown on the website.
    Pair {
        /// The server's URL, or the whole replique://pair?… link.
        server: String,
        /// The pairing code (not needed with a link).
        code: Option<String>,
        /// Keep an existing SSH key and restic password (a repository that
        /// already exists) instead of generating new ones.
        #[arg(long, num_args = 2, value_names = ["KEY_FILE", "PASSWORD_FILE"])]
        adopt: Option<Vec<PathBuf>>,
        /// The name this device gives itself (the website's name wins).
        #[arg(long)]
        name: Option<String>,
        /// Do not wait for confirmation that the new password was saved.
        #[arg(long)]
        yes: bool,
        /// Do not open or create the repository after pairing.
        #[arg(long)]
        no_init: bool,
        /// Pair again over an existing pairing.
        #[arg(long)]
        force: bool,
    },
    /// Run a job: a backup with its prune and check, or a mirror.
    Run { job: String },
    /// Run a mirror job (same as `run` for one).
    Mirror { job: String },
    /// Show the last runs of each job.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Run restic on a job's repository, e.g. `replique-client restic hetzner snapshots`.
    Restic {
        job: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Write a job's exclude file (configured and generated) and print it.
    Excludes { job: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match real_main(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            log!("{e}");
            ExitCode::from(1)
        }
    }
}

fn real_main(cli: Cli) -> Result<i32, Error> {
    // Pairing comes before the module is set up, and `status` is useful either
    // way; everything else needs its job's configuration.
    let optional = matches!(cli.command, Cmd::Pair { .. } | Cmd::Status { .. });
    let config = ClientConfig::load(&cli.config, !optional)?;
    let state = State::new(
        cli.state_dir
            .clone()
            .or_else(|| config.state_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
    );
    match cli.command {
        Cmd::Pair {
            server,
            code,
            adopt,
            name,
            yes,
            no_init,
            force,
        } => {
            let (server, code) = PairOptions::target(&server, code.as_deref())?;
            let adopt = adopt.map(|v| (v[0].clone(), v[1].clone()));
            pair::pair(
                &config,
                &state,
                &PairOptions {
                    server,
                    code,
                    adopt,
                    name,
                    yes,
                    init: !no_init,
                    force,
                },
            )?;
            Ok(0)
        }
        Cmd::Run { job } => run::run(&config, &state, &job, &Stop::install()),
        Cmd::Mirror { job } => {
            if config.job(&job)?.kind != JobKind::Mirror {
                return Err(Error::msg(format!("{job} is not a mirror job; use `run`")));
            }
            run::run(&config, &state, &job, &Stop::install())
        }
        Cmd::Status { json } => {
            if json {
                let mut all = serde_json::Map::new();
                for job in config.jobs.keys() {
                    all.insert(
                        job.clone(),
                        serde_json::to_value(status::load(&state, job))?,
                    );
                }
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else {
                print!("{}", status::render(&config, &state));
            }
            Ok(0)
        }
        Cmd::Restic { job, args } => {
            let j = config.job(&job)?;
            let restic = match j.kind {
                JobKind::Backup => {
                    if j.repository.is_none() {
                        pair::ensure_ssh_config(&config, &state);
                    }
                    Restic::for_job(&config, &state, j)?
                }
                JobKind::Mirror => Restic::for_mirror(j)?,
            };
            use std::os::unix::process::CommandExt;
            let err = restic.command(&args).exec();
            Err(Error::msg(format!(
                "cannot run {}: {err}",
                restic.binary.display()
            )))
        }
        Cmd::Excludes { job } => {
            let j = config.job(&job)?;
            let path = run::write_excludes(&state, &job, j)?;
            print!("{}", std::fs::read_to_string(&path)?);
            Ok(0)
        }
    }
}
