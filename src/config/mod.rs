use std::{net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Puxiri — self-hosted work tracking (development spike)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the local development server. Product authentication is not implemented yet.
    Serve {
        #[arg(long, env = "PUXIRI_BIND", default_value = "127.0.0.1:3000")]
        bind: SocketAddr,
        #[arg(long, env = "PUXIRI_DATA_DIR", default_value = "data")]
        data_dir: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_options_are_parsed_without_loading_functional_settings() {
        let cli = Cli::try_parse_from([
            "puxiri",
            "serve",
            "--bind",
            "127.0.0.1:4567",
            "--data-dir",
            "a directory",
        ])
        .unwrap();
        let Command::Serve { bind, data_dir } = cli.command;
        assert_eq!(bind.port(), 4567);
        assert!(bind.ip().is_loopback());
        assert_eq!(data_dir, PathBuf::from("a directory"));
    }

    #[test]
    fn invalid_bind_is_rejected() {
        assert!(Cli::try_parse_from(["puxiri", "serve", "--bind", "invalid"]).is_err());
    }

    #[test]
    fn unimplemented_commands_are_not_advertised_as_working() {
        assert!(Cli::try_parse_from(["puxiri", "admin", "recover"]).is_err());
    }
}
