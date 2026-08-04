mod manifest;
mod resolver;
mod supervisor;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::Response,
    routing::any,
};
use clap::Parser;
use supervisor::Supervisor;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Directory containing one subdirectory per domain.
    #[arg(long, env = "MULTI_SERVER_ROOT", default_value = "/raiz")]
    root: PathBuf,

    /// Public address on which the domain router listens.
    #[arg(long, env = "MULTI_SERVER_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "multi_server=info".into()),
        )
        .init();

    let args = Args::parse();
    let supervisor = Arc::new(Supervisor::new(args.root, reqwest::Client::new()));
    let app = Router::new().fallback(any(dispatch)).with_state(supervisor);
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind {}", args.listen))?;
    info!(address = %args.listen, "multi-server listening");
    axum::serve(listener, app)
        .await
        .context("HTTP server stopped")
}

async fn dispatch(State(supervisor): State<Arc<Supervisor>>, request: Request<Body>) -> Response {
    match supervisor.handle(request).await {
        Ok(response) => response,
        Err(error) => {
            error!(%error, "request failed");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("multi-server: {error}\n")))
                .expect("valid error response")
        }
    }
}
