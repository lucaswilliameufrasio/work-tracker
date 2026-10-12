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
        #[arg(long = "master-key", env = "PUXIRI_MASTER_KEY")]
        master_key_path: Option<PathBuf>,
    },
    /// Issue and display a one-time installation setup token to a local terminal.
    SetupToken {
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
        let Command::Serve {
            bind,
            data_dir,
            master_key_path,
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(bind.port(), 4567);
        assert!(bind.ip().is_loopback());
        assert_eq!(data_dir, PathBuf::from("a directory"));
        assert_eq!(master_key_path, None);
    }

    #[test]
    fn master_key_path_can_be_configured_explicitly() {
        let cli = Cli::try_parse_from(["puxiri", "serve", "--master-key", "secrets/instance.key"])
            .unwrap();
        let Command::Serve {
            master_key_path, ..
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(master_key_path, Some(PathBuf::from("secrets/instance.key")));
    }

    #[test]
    fn invalid_bind_is_rejected() {
        assert!(Cli::try_parse_from(["puxiri", "serve", "--bind", "invalid"]).is_err());
    }

    #[test]
    fn unimplemented_commands_are_not_advertised_as_working() {
        assert!(Cli::try_parse_from(["puxiri", "admin", "recover"]).is_err());
    }

    #[test]
    fn setup_token_command_uses_the_configured_data_directory() {
        let cli =
            Cli::try_parse_from(["puxiri", "setup-token", "--data-dir", "a directory"]).unwrap();
        let Command::SetupToken { data_dir } = cli.command else {
            panic!("expected setup-token command");
        };
        assert_eq!(data_dir, PathBuf::from("a directory"));
    }
}
