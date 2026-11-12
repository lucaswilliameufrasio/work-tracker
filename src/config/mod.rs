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
        /// Enable browser session and read-only project APIs for this explicit HTTPS origin.
        #[arg(long, env = "PUXIRI_BROWSER_ORIGIN")]
        browser_origin: Option<crate::http::browser_session::BrowserOrigin>,
        /// Local sqlite-replication repository. Another filesystem is required to survive loss of the data disk.
        #[arg(long, env = "PUXIRI_REPLICATION_DIR")]
        replication_dir: Option<PathBuf>,
        #[command(flatten)]
        replication_s3: Box<ReplicationS3Args>,
        #[command(flatten)]
        replication_schedule: ReplicationScheduleArgs,
    },
    /// Issue and display a one-time installation setup token to a local terminal.
    SetupToken {
        #[arg(long, env = "PUXIRI_DATA_DIR", default_value = "data")]
        data_dir: PathBuf,
    },
}

/// Scheduled capture and how much replication history is kept.
#[derive(Debug, Clone, Copy, clap::Args)]
pub struct ReplicationScheduleArgs {
    /// Seconds between automatic captures. 0 leaves capture to an administrator.
    #[arg(
        long = "replication-interval-seconds",
        env = "PUXIRI_REPLICATION_INTERVAL_SECONDS",
        default_value_t = 300
    )]
    pub interval_seconds: u64,
    /// Hours after which the next capture starts a new generation from a fresh snapshot.
    #[arg(
        long = "replication-generation-hours",
        env = "PUXIRI_REPLICATION_GENERATION_HOURS",
        default_value_t = 24,
        value_parser = clap::value_parser!(u64).range(1..=8760)
    )]
    pub generation_hours: u64,
    /// Generations kept at the S3 destination, the current one included.
    #[arg(
        long = "replication-retain-generations",
        env = "PUXIRI_REPLICATION_RETAIN_GENERATIONS",
        default_value_t = 7,
        value_parser = clap::value_parser!(u64).range(1..=1000)
    )]
    pub retained_generations: u64,
}

impl ReplicationScheduleArgs {
    pub fn policy(self) -> crate::replication::ReplicationPolicy {
        crate::replication::ReplicationPolicy {
            capture_interval: (self.interval_seconds > 0)
                .then(|| std::time::Duration::from_secs(self.interval_seconds)),
            generation_age: std::time::Duration::from_secs(self.generation_hours * 60 * 60),
            retained_generations: std::num::NonZeroUsize::new(
                usize::try_from(self.retained_generations).unwrap_or(usize::MAX),
            )
            .unwrap_or(std::num::NonZeroUsize::MIN),
        }
    }
}

/// Optional S3-compatible destination for each replication capture.
///
/// Credentials are read only from `PUXIRI_REPLICATION_S3_ACCESS_KEY_ID` and
/// `PUXIRI_REPLICATION_S3_SECRET_ACCESS_KEY`, never from command-line flags.
#[derive(Debug, Clone, clap::Args)]
pub struct ReplicationS3Args {
    /// Bucket that receives the replication chain. Uploads stay off when unset.
    #[arg(long = "replication-s3-bucket", env = "PUXIRI_REPLICATION_S3_BUCKET")]
    pub bucket: Option<String>,
    /// Key prefix inside the bucket, reserved for this installation.
    #[arg(
        long = "replication-s3-prefix",
        env = "PUXIRI_REPLICATION_S3_PREFIX",
        default_value = "puxiri"
    )]
    pub prefix: String,
    #[arg(
        long = "replication-s3-region",
        env = "PUXIRI_REPLICATION_S3_REGION",
        default_value = "us-east-1"
    )]
    pub region: String,
    /// S3-compatible endpoint URL. HTTPS is required unless the host is loopback.
    #[arg(
        long = "replication-s3-endpoint",
        env = "PUXIRI_REPLICATION_S3_ENDPOINT"
    )]
    pub endpoint: Option<String>,
    /// Address the bucket in the path instead of the host name.
    #[arg(
        long = "replication-s3-path-style",
        env = "PUXIRI_REPLICATION_S3_PATH_STYLE"
    )]
    pub path_style: bool,
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
            replication_dir,
            replication_s3,
            replication_schedule,
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(bind.port(), 4567);
        assert!(bind.ip().is_loopback());
        assert_eq!(data_dir, PathBuf::from("a directory"));
        assert_eq!(master_key_path, None);
        assert_eq!(browser_origin, None);
        assert_eq!(replication_dir, None);
        assert_eq!(replication_s3.bucket, None);
        assert_eq!(replication_s3.prefix, "puxiri");
        assert!(!replication_s3.path_style);
        assert_eq!(
            replication_schedule.policy(),
            crate::replication::ReplicationPolicy::default()
        );
    }

    #[test]
    fn replication_schedule_can_be_disabled_and_rejects_zero_retention() {
        let cli = Cli::try_parse_from([
            "puxiri",
            "serve",
            "--replication-interval-seconds",
            "0",
            "--replication-generation-hours",
            "6",
            "--replication-retain-generations",
            "2",
        ])
        .unwrap();
        let Command::Serve {
            replication_schedule,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };
        let policy = replication_schedule.policy();
        assert_eq!(policy.capture_interval, None);
        assert_eq!(policy.generation_age.as_secs(), 6 * 60 * 60);
        assert_eq!(policy.retained_generations.get(), 2);
        for (flag, value) in [
            ("--replication-retain-generations", "0"),
            ("--replication-generation-hours", "0"),
        ] {
            assert!(Cli::try_parse_from(["puxiri", "serve", flag, value]).is_err());
        }
    }

    #[test]
    fn s3_destination_is_parsed_without_credential_flags() {
        let cli = Cli::try_parse_from([
            "puxiri",
            "serve",
            "--replication-s3-bucket",
            "replica",
            "--replication-s3-endpoint",
            "http://127.0.0.1:9000",
            "--replication-s3-path-style",
        ])
        .unwrap();
        let Command::Serve { replication_s3, .. } = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(replication_s3.bucket.as_deref(), Some("replica"));
        assert_eq!(
            replication_s3.endpoint.as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert!(replication_s3.path_style);
        assert!(
            Cli::try_parse_from(["puxiri", "serve", "--replication-s3-secret-access-key", "x"])
                .is_err()
        );
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
