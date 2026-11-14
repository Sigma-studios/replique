use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use replique_server::config::Config;
use replique_server::{App, auth, ssh};

#[derive(Parser)]
#[command(name = "replique-server", version, about = "Réplique's server")]
struct Cli {
    /// The configuration file.
    #[arg(
        long,
        short,
        env = "REPLIQUE_CONFIG",
        default_value = "/etc/replique/replique.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the server (the default).
    Serve,
    /// Reads a password on stdin and prints its argon2 hash, for
    /// `admin_password_hash_file`.
    HashPassword,
    /// Checks the configuration file and exits.
    CheckConfig,
    /// Speaks SFTP on stdin and stdout to a Storage Box login; restic runs this
    /// as its `sftp.command` during website restores.
    SftpBridge {
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 23)]
        port: u16,
        #[arg(long)]
        user: String,
        /// The private key to log in with.
        #[arg(long)]
        key: PathBuf,
        /// The host key to require (`alg base64`).
        #[arg(long)]
        host_key: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Logs go to stderr: the bridge's stdout is an SFTP stream.
    let default = match cli.command {
        Some(Command::SftpBridge { .. }) => "warn",
        _ => "info,russh=warn,russh_sftp=warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("REPLIQUE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let runtime = tokio::runtime::Runtime::new()?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            let config = Config::load(&cli.config)?;
            let app = Arc::new(App::new(config)?);
            runtime.block_on(replique_server::serve(app))
        }
        Command::CheckConfig => {
            let config = Config::load(&cli.config)?;
            println!(
                "{} is valid: listening on {}, state in {}",
                cli.config.display(),
                config.listen,
                config.state_dir.display()
            );
            Ok(())
        }
        Command::HashPassword => {
            eprintln!("Password (read from stdin):");
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let password = line.trim_end_matches(['\n', '\r']);
            if password.len() < 12 {
                anyhow::bail!("use at least 12 characters");
            }
            println!("{}", auth::hash_password(password)?);
            Ok(())
        }
        Command::SftpBridge {
            host,
            port,
            user,
            key,
            host_key,
        } => runtime.block_on(async move {
            let raw = std::fs::read_to_string(&key)
                .with_context(|| format!("reading {}", key.display()))?;
            let key = russh::keys::ssh_key::PrivateKey::from_openssh(raw)?;
            let trust = ssh::Trust::from_lines([host_key.as_str()], &host, port);
            anyhow::ensure!(!trust.keys.is_empty(), "--host-key is not a public key");
            let endpoint = ssh::Endpoint {
                host,
                port,
                username: user,
            };
            let conn = ssh::connect(&endpoint, ssh::Auth::Key(&key), trust).await?;
            ssh::bridge(conn).await
        }),
    }
}
