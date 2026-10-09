use clap::Parser;
use item_mcp::ItemMcpServer;
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(name = "item-mcp", version, about = "Item Finders MCP stdio server")]
struct Args {
    /// Use stdio transport (also the default).
    #[arg(long)]
    stdio: bool,

    /// Existing SQLite database written by item-ingest; opened read-only per query.
    #[arg(long, env = "ITEM_DB", default_value = "data/items.db")]
    db: std::path::PathBuf,

    /// Startup-only TOML priors (maximum 1 MiB). CLI overrides ITEM_PRIORS; absent uses builtin priors.
    #[arg(long, env = "ITEM_PRIORS", hide_env_values = true)]
    priors: Option<std::path::PathBuf>,

    /// Local daemon HTTP origin, e.g. http://127.0.0.1:8480.
    #[arg(long, env = "ITEM_DAEMON_ENDPOINT")]
    daemon_endpoint: Option<String>,

    /// Bearer token for daemon images and frame freezes. Prefer ITEM_DAEMON_TOKEN over command-line secrets.
    #[arg(long, env = "ITEM_DAEMON_TOKEN", hide_env_values = true)]
    daemon_token: Option<String>,

    /// Register annotation submission. Requires endpoint, read token and distinct ITEM_DAEMON_ANNOTATION_TOKEN.
    #[arg(long)]
    enable_annotations: bool,

    /// Deadline for one complete daemon tool request, including database lookup, in seconds.
    #[arg(long, default_value_t = 15)]
    daemon_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(!args.db.as_os_str().is_empty(), "--db must not be empty");
    let database_path = if args.db.is_absolute() {
        args.db
    } else {
        std::env::current_dir()?.join(args.db)
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("starting item-mcp stdio query and camera frame server");
    let priors = match args.priors {
        Some(path) => item_mcp::priors::PriorsSnapshot::load(&path)?,
        None => item_mcp::priors::PriorsSnapshot::default(),
    };
    let mut server = ItemMcpServer::new_with_priors(database_path, priors);
    anyhow::ensure!(
        !args.enable_annotations || args.daemon_endpoint.is_some(),
        "--enable-annotations requires a daemon endpoint and read token"
    );
    if let Some(endpoint) = args.daemon_endpoint {
        let token = args.daemon_token.as_deref().ok_or_else(|| {
            anyhow::anyhow!("daemon endpoint requires --daemon-token or ITEM_DAEMON_TOKEN")
        })?;
        let mut daemon = item_mcp::images::DaemonClient::new(
            &endpoint,
            token,
            std::time::Duration::from_secs(args.daemon_timeout_secs),
        )?;
        if args.enable_annotations {
            let token = std::env::var("ITEM_DAEMON_ANNOTATION_TOKEN").map_err(|_| {
                anyhow::anyhow!("--enable-annotations requires ITEM_DAEMON_ANNOTATION_TOKEN")
            })?;
            daemon = daemon.with_annotations(&token)?;
        }
        server = server.with_daemon(daemon);
    }
    let service = server.serve(item_mcp::transport::stdio()).await?;
    service.waiting().await?;
    tracing::info!("item-mcp stdio transport closed");
    Ok(())
}
