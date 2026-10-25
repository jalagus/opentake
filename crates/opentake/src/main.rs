mod gui;
mod open_files;
mod server;
mod settings;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{Parser, Subcommand};
use opentake_engine::{AudioConfig, Engine};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tracing_subscriber::EnvFilter;

use server::DawServer;

#[derive(Parser)]
#[command(name = "opentake", version, about = "OpenTake — an open-source DAW you can drive with AI agents over MCP")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// MCP over streamable HTTP on this address. With `serve` this replaces stdio;
    /// with the GUI it defaults to 127.0.0.1:8808.
    #[arg(long, global = true)]
    http: Option<SocketAddr>,

    /// GUI only: don't start the MCP server.
    #[arg(long, global = true)]
    no_mcp: bool,

    /// Open this project (.otp, .rpp, .json, or one to import: .dawproject, .als, .fcpxml, .ardour) on startup.
    #[arg(long, global = true)]
    project: Option<PathBuf>,

    /// A file to open: a project (.otp, .rpp, .json, .dawproject, .als, .fcpxml, .ardour), or
    /// with the GUI a Guitar Pro song, MIDI file or WAV to import (this is how "Open with" and
    /// file associations launch OpenTake).
    file: Option<PathBuf>,

    /// Start audio with default devices on startup.
    #[arg(long, global = true)]
    start_audio: bool,

    /// Open output only on startup (no input device, so no recording/monitoring).
    #[arg(long, global = true)]
    no_input: bool,

    /// Additional VST3 search folder (repeatable).
    #[arg(long = "plugin-path", global = true)]
    plugin_paths: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the GUI with an MCP HTTP server (default).
    Gui,
    /// Run headless, serving MCP over stdio (or HTTP with --http). Use this when an
    /// agent launches the process itself.
    Serve,
    /// Print audio devices and exit.
    Devices,
    /// Scan for VST3 plugins (and Audio Units on macOS) and exit.
    Scan,
}

const DEFAULT_GUI_MCP_ADDR: &str = "127.0.0.1:8808";

fn main() -> Result<()> {
    // stdout belongs to the MCP stdio transport, so log to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    let mut cli = Cli::parse();
    // A project passed as a plain argument opens like --project; other files go to the GUI.
    if let Some(file) = cli.file.take() {
        if cli.project.is_none() && opentake_engine::is_project_path(&file) {
            cli.project = Some(file);
        } else {
            open_files::push(file);
        }
    }
    let mut engine = Engine::new();
    // Saved scan folders (from the Plug-in Manager); --plugin-path adds to them for this run.
    let settings_path = settings::default_path();
    if let Some(paths) = settings_path.as_deref().map(settings::Settings::load).and_then(|s| s.plugin_paths) {
        engine.set_plugin_search_paths(paths);
    }
    // Recording latency offsets per device setup live next to the settings.
    if let Some(dir) = settings_path.as_deref().and_then(|p| p.parent()) {
        engine.set_latency_store(&dir.join("latency.json"));
    }
    for p in &cli.plugin_paths {
        engine.add_plugin_search_path(p.clone());
    }

    let command = cli.command.unwrap_or(Cmd::Gui);
    match command {
        Cmd::Devices => {
            println!("{}", serde_json::to_string_pretty(&engine.list_devices()?)?);
            return Ok(());
        }
        Cmd::Scan => {
            let (plugins, errors) = engine.scan_plugins();
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "plugins": plugins, "audio_units": engine.audio_units(), "errors": errors }))?);
            return Ok(());
        }
        Cmd::Gui | Cmd::Serve => {}
    }

    let gui = matches!(command, Cmd::Gui);
    if gui {
        // Before the app finishes launching, so a double-clicked document isn't missed.
        open_files::install();
    }
    // The GUI reopens the last project unless one was given on the command line.
    let last_project = (gui && cli.project.is_none())
        .then(|| settings_path.as_deref().map(settings::Settings::load).and_then(|s| s.existing_recent().into_iter().next()))
        .flatten();
    if let Some(path) = cli.project.as_ref().or(last_project.as_ref()) {
        match engine.load_project(path) {
            Ok(warnings) => warnings.iter().for_each(|w| tracing::warn!("{w}")),
            // A broken last project shouldn't stop the app from starting.
            Err(e) if cli.project.is_none() => tracing::warn!("could not reopen {}: {e:#}", path.display()),
            Err(e) => return Err(e),
        }
    }
    if cli.start_audio || gui {
        // The GUI starts audio by default so it is immediately usable.
        let config = AudioConfig { disable_input: cli.no_input, ..AudioConfig::default() };
        if let Err(e) = engine.start_audio(&config) {
            tracing::warn!("could not start audio: {e:#}");
        }
    }
    if gui {
        // Populate the plugin browser; failures are reported per bundle.
        engine.scan_plugins();
        // Start with one empty track rather than a blank arrangement.
        if engine.project().tracks.is_empty() {
            engine.add_track("Audio 1")?;
        }
    }

    let engine = Arc::new(Mutex::new(engine));
    let runtime = tokio::runtime::Runtime::new()?;

    if gui {
        let mcp_url = if cli.no_mcp {
            None
        } else {
            let addr = cli.http.unwrap_or_else(|| DEFAULT_GUI_MCP_ADDR.parse().expect("valid default address"));
            let listener = runtime.block_on(tokio::net::TcpListener::bind(addr))?;
            let url = format!("http://{addr}/mcp");
            tracing::info!("serving MCP over HTTP at {url}");
            runtime.spawn(serve_http(engine.clone(), listener));
            Some(url)
        };
        // eframe must own the main thread (required on macOS).
        return gui::run(engine, mcp_url, settings_path);
    }

    runtime.block_on(async move {
        match cli.http {
            Some(addr) => {
                let listener = tokio::net::TcpListener::bind(addr).await?;
                tracing::info!("serving MCP over HTTP at http://{addr}/mcp");
                serve_http(engine, listener).await
            }
            None => {
                tracing::info!("serving MCP over stdio");
                DawServer::new(engine).serve(stdio()).await?.waiting().await?;
                Ok(())
            }
        }
    })
}

async fn serve_http(engine: server::SharedEngine, listener: tokio::net::TcpListener) -> Result<()> {
    let service: StreamableHttpService<DawServer, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(DawServer::new(engine.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
