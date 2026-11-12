use std::{error::Error, io::IsTerminal, sync::Arc};

use chrono::Utc;
use clap::Parser;
use puxiri::{
    auth::{master_key::MasterKeyStore, session::SessionPolicy, setup::SetupTokenStore},
    config::{Cli, Command},
    db::Database,
    http,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().command {
        Command::Serve {
            bind,
            data_dir,
            master_key_path,
            browser_origin,
            replication_dir,
            replication_s3,
            replication_schedule,
        } => {
            let remote_replica = remote_replica(*replication_s3)?;
            let database = match replication_dir {
                Some(path) => Database::open_with_replication(&data_dir, &path).await?,
                None => Database::open(&data_dir).await?,
            };
            let database = database.with_replication_policy(replication_schedule.policy());
            let database = Arc::new(match remote_replica {
                Some(remote) => {
                    tracing::info!(location = %remote.location(), "replication uploads to S3");
                    database.with_remote_replica(remote)
                }
                None => database,
            });
            let master_key_path = master_key_path.unwrap_or_else(|| data_dir.join("master.key"));
            let master_key = MasterKeyStore::load_or_create(&master_key_path)?;
            let browser_entry = browser_origin.is_some();
            let router = match browser_origin {
                Some(origin) => http::router_with_entry(
                    database.clone(),
                    http::browser_session::BrowserSessionSecurity::from_origin(origin, master_key),
                    SessionPolicy::initial(chrono::TimeDelta::minutes(5))?,
                    Arc::new(puxiri::auth::muira_exchange::HttpMuiraExchange),
                ),
                None => http::router(database.clone()),
            };
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(
                address = %listener.local_addr()?,
                browser_entry,
                "development server listening"
            );
            let (stop_replication, replication_shutdown) = tokio::sync::watch::channel(false);
            let scheduler =
                puxiri::replication::spawn_scheduler(database.clone(), replication_shutdown);
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await;
            // A capture in progress finishes; no new one starts.
            let _ = stop_replication.send(true);
            if let Some(scheduler) = scheduler
                && let Err(error) = scheduler.await
            {
                tracing::warn!(%error, "replication scheduler stopped abnormally");
            }
            database.close().await;
            served?;
            Ok(())
        }
        Command::SetupToken { data_dir } => {
            if !std::io::stdout().is_terminal() {
                return Err("setup token is only displayed to an interactive terminal".into());
            }
            let database = Database::open(&data_dir).await?;
            let token = SetupTokenStore::new(database.pool().clone())
                .issue(Utc::now())
                .await?;
            println!("One-time setup token (save it now; it will not be shown again):");
            println!("{}", token.expose());
            database.close().await;
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

fn remote_replica(
    args: puxiri::config::ReplicationS3Args,
) -> Result<Option<puxiri::replication::RemoteReplica>, Box<dyn Error>> {
    // Compose and env files pass unset settings as empty strings.
    let Some(bucket) = args.bucket.filter(|bucket| !bucket.trim().is_empty()) else {
        return Ok(None);
    };
    let credential = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{name} is required when the replication S3 bucket is set"))
    };
    let settings = puxiri::replication::RemoteSettings {
        bucket,
        prefix: args.prefix,
        region: args.region,
        endpoint: args.endpoint.filter(|endpoint| !endpoint.trim().is_empty()),
        path_style: args.path_style,
        access_key_id: credential("PUXIRI_REPLICATION_S3_ACCESS_KEY_ID")?,
        secret_access_key: credential("PUXIRI_REPLICATION_S3_SECRET_ACCESS_KEY")?,
    };
    Ok(Some(puxiri::replication::RemoteReplica::new(settings)?))
}
