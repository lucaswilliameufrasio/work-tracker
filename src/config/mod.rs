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
        /// Enable read-only browser session APIs for this explicit HTTPS origin.
        #[arg(long, env = "PUXIRI_BROWSER_ORIGIN")]
        browser_origin: Option<crate::http::browser_session::BrowserOrigin>,
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
            browser_origin,
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(bind.port(), 4567);
        assert!(bind.ip().is_loopback());
        assert_eq!(data_dir, PathBuf::from("a directory"));
        assert_eq!(master_key_path, None);
        assert_eq!(browser_origin, None);
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
    fn browser_origin_is_explicit_canonical_and_rejects_insecure_or_malformed_input() {
        let cli = Cli::try_parse_from([
            "puxiri",
            "serve",
            "--browser-origin",
            "https://TRACKER.example.test:443/",
        ])
        .unwrap();
        let Command::Serve { browser_origin, .. } = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(
            browser_origin.unwrap().as_str(),
            "https://tracker.example.test"
        );
        for origin in [
            "http://tracker.example.test",
            "https://tracker.example.test/path",
            "https://tracker.example.test:99999",
        ] {
            assert!(Cli::try_parse_from(["puxiri", "serve", "--browser-origin", origin]).is_err());
        }
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
