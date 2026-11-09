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
        } => {
            let database = match replication_dir {
                Some(path) => Arc::new(Database::open_with_replication(&data_dir, &path).await?),
                None => Arc::new(Database::open(&data_dir).await?),
            };
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
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            database.close().await;
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
