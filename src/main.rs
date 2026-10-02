use clap::{Parser, Subcommand};
use std::{future::IntoFuture, path::PathBuf};
use switchyard::app::{AppState, router};
#[derive(Parser)]
#[command(
    version,
    about = "A local-first Rust AI gateway. One endpoint. Your models."
)]
struct Cli {
    #[arg(long, env = "SWITCHYARD_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "SWITCHYARD_PORT", default_value_t = 7410)]
    port: u16,
    #[arg(long, env = "SWITCHYARD_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 64)]
    max_in_flight: usize,
    #[arg(long, default_value_t = 300)]
    timeout: u64,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    /// Print the path of the private admin token file.
    TokenPath,
    /// Import a signed-in local CLI account without modifying its auth store.
    Import {
        #[arg(value_parser=["codex","claude","cliproxy"])]
        source: String,
        #[arg(long)]
        path: Option<String>,
    },
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "switchyard=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if cli.max_in_flight == 0 || cli.max_in_flight > 4096 || cli.timeout == 0 || cli.timeout > 86400
    {
        return Err("Invalid concurrency/timeout limits".into());
    }
    let data = cli.data_dir.unwrap_or_else(|| {
        if let Ok(x) = std::env::var("XDG_DATA_HOME") {
            PathBuf::from(x).join("switchyard")
        } else {
            PathBuf::from(
                std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| ".".into()),
            )
            .join(".local/share/switchyard")
        }
    });
    if matches!(cli.command, Some(Commands::TokenPath)) {
        println!("{}", data.join("admin-token").display());
        return Ok(());
    }
    let app = match AppState::new(
        data.clone(),
        cli.host.clone(),
        cli.port,
        cli.max_in_flight,
        cli.timeout,
    ) {
        Ok(app) => app,
        Err(error) if error.is::<switchyard::app::InstanceInUse>() => {
            if let Some(Commands::Import { source, path }) = &cli.command {
                import_running(&data, cli.port, source, path.as_deref()).await?;
                return Ok(());
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let _ = std::fs::remove_file(data.join("runtime.json"));
    match cli.command {
        Some(Commands::TokenPath) => {
            println!("{}", data.join("admin-token").display());
            return Ok(());
        }
        Some(Commands::Import { source, path }) => {
            let cs = switchyard::credentials::import(&app, &source, path.as_deref())
                .await
                .map_err(|e| e.message)?;
            println!(
                "Imported {} connection(s). Original credentials were not changed.",
                cs.len()
            );
            return Ok(());
        }
        None => {}
    }
    let listener = tokio::net::TcpListener::bind((cli.host.as_str(), cli.port)).await?;
    let address = listener.local_addr()?;
    let endpoint = serde_json::json!({"pid":std::process::id(),"port":address.port(),"host":address.ip().to_string()});
    let temporary = data.join(format!(".runtime-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(&temporary)?;
    file.write_all(serde_json::to_string(&endpoint)?.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, data.join("runtime.json"))?;
    tracing::info!(address=%listener.local_addr()?,"Switchyard ready. Open the dashboard in your browser.");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result=&mut server=>{result?;},
        _=shutdown_signal()=>{let _=stop_tx.send(());if tokio::time::timeout(std::time::Duration::from_secs(10),&mut server).await.is_err(){tracing::info!("Shutdown drain deadline reached");}}
    }
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn import_running(
    data: &std::path::Path,
    port: u16,
    source: &str,
    path: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let runtime: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(data.join("runtime.json"))
            .map_err(|_| "Gateway is starting; retry the import in a moment")?,
    )?;
    if runtime["port"].as_u64() != Some(port as u64) {
        return Err(
            "The running gateway uses another port; use --port with its listening port".into(),
        );
    }
    let host = match runtime["host"]
        .as_str()
        .ok_or("Invalid gateway runtime file")?
    {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        host => host,
    };
    let host: std::net::IpAddr = host.parse()?;
    let address = std::net::SocketAddr::new(host, port);
    let token = std::fs::read_to_string(data.join("admin-token"))?;
    let path = path
        .map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                Ok(p)
            } else {
                std::env::current_dir().map(|cwd| cwd.join(p))
            }
        })
        .transpose()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = client
        .post(format!("http://{address}/api/import"))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({"source":source,"path":path}))
        .send()
        .await
        .map_err(|_| "Gateway import interrupted; check Connections before retrying")?;
    let status = response.status();
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err("Gateway import response exceeded the size limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    if !status.is_success() {
        let message = value["error"]["message"]
            .as_str()
            .unwrap_or("Gateway declined import");
        return Err(format!(
            "{} (HTTP {})",
            message.replace(token.trim(), "[redacted]"),
            status
        )
        .into());
    }
    println!(
        "Imported {} connection(s). Original credentials were not changed.",
        value["imported"].as_u64().unwrap_or(0)
    );
    Ok(())
}
