use anyhow::{ensure, Context, Result};
use factories::{
    api::{self, App, Identity},
    config::Platform,
    execution::{dispatch_one, Executor},
    storage::Blobs,
    store::Store,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(env(
            "RUST_LOG",
            "factories=info,factory_server=info,tower_http=info",
        ))
        .init();
    let platform = Platform::load(&PathBuf::from(env(
        "FACTORY_CONFIG",
        "config/platform.yaml",
    )))?;
    let snapshot = platform.snapshot(
        &PathBuf::from(env("FACTORY_WORKFLOWS", "workflows")),
        &PathBuf::from(env("FACTORY_PROMPTS", "prompts")),
    )?;
    let secret = std::env::var("FACTORY_WORKER_SECRET")
        .context("FACTORY_WORKER_SECRET is required; keep it stable across restarts")?;
    ensure!(
        secret.len() >= 32,
        "worker secret must be at least 32 characters"
    );
    let identities: Vec<Identity> = serde_json::from_str(
        &std::env::var("FACTORY_IDENTITIES").context("FACTORY_IDENTITIES is required")?,
    )?;
    ensure!(
        !identities.is_empty()
            && identities
                .iter()
                .all(|i| i.token.len() >= 24 && !i.subject.is_empty()),
        "at least one named API identity with a 24-character token is required"
    );
    let executor = Executor {
        mode: env("FACTORY_EXECUTOR", "docker"),
        server_url: env("FACTORY_WORKER_SERVER_URL", "http://server:8080"),
        network: env("FACTORY_DOCKER_NETWORK", "factory_workers"),
        worker_binary: PathBuf::from(env(
            "FACTORY_WORKER_BINARY",
            if cfg!(windows) {
                "target/debug/factory-worker.exe"
            } else {
                "target/debug/factory-worker"
            },
        )),
        secret,
        ecs_cluster: env("FACTORY_ECS_CLUSTER", ""),
        ecs_network: env("FACTORY_ECS_NETWORK", "{}"),
    };
    ensure!(
        matches!(executor.mode.as_str(), "docker" | "ecs" | "process"),
        "unknown executor"
    );
    ensure!(
        executor.mode != "process" || platform.allow_fixture,
        "process mode is restricted to development fixtures"
    );
    if executor.mode == "ecs" {
        ensure!(
            reqwest::Url::parse(&executor.server_url)?.scheme() == "https",
            "ECS worker callbacks require HTTPS"
        );
        ensure!(
            env("FACTORY_PUBLIC_READ", "false") != "true",
            "ECS deployments require authenticated observation"
        );
    }
    let store =
        Store::connect(&std::env::var("DATABASE_URL").context("DATABASE_URL is required")?).await?;
    let app = Arc::new(App {
        store,
        platform,
        snapshot,
        executor,
        blobs: Blobs {
            root: PathBuf::from(env("FACTORY_ARTIFACT_ROOT", "data/artifacts")),
            bucket: std::env::var("FACTORY_S3_BUCKET").ok(),
        },
        http: reqwest::Client::builder()
            .user_agent("factories/0.1")
            .timeout(Duration::from_secs(25))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        identities,
        public_read: env("FACTORY_PUBLIC_READ", "false") == "true",
    });
    let coordinator = app.clone();
    let task = tokio::spawn(async move {
        loop {
            if let Err(error) = coordinator.store.reconcile().await {
                tracing::error!(%error,"coordinator reconciliation failed");
            }
            for _ in 0..20 {
                match dispatch_one(
                    &coordinator.store,
                    &coordinator.executor,
                    &coordinator.platform,
                    &coordinator.http,
                )
                .await
                {
                    Ok(true) => (),
                    Ok(false) => break,
                    Err(error) => {
                        tracing::error!(%error,"outbox dispatch failed");
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(env("FACTORY_LISTEN", "127.0.0.1:8080")).await?;
    tracing::info!(address=%listener.local_addr()?,"Factories portal ready");
    axum::serve(listener, api::router(app, &env("FACTORY_WEB_ROOT", "web")))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    task.abort();
    Ok(())
}
