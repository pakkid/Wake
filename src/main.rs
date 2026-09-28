use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use wake::api::AppState;
use wake::config::{AllowList, Config};
use wake::grub::{self, GrubContext};
use wake::probe::{Monitor, Prober};
use wake::state::BootService;
use wake::store::Store;
use wake::wol::UdpWol;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    if let Err(e) = run().await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cfg = Arc::new(Config::from_env().context("configuration error")?);

    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        tracing::error!(dir = %cfg.data_dir.display(), error = %e, "can't create the data directory");
    }

    let revision = wake::new_revision();
    let boot = Arc::new(
        BootService::load(
            Store::new(&cfg.data_dir),
            cfg.boot_settings(),
            revision.clone(),
        )
        .await,
    );
    if !boot.storage_ok() {
        tracing::error!(
            dir = %cfg.data_dir.display(),
            "state can't be saved; choices will be lost on restart. Check the volume's permissions."
        );
    }

    let prober = cfg.pc_ip.map(|ip| Prober::new(ip, cfg.probes.clone()));
    let monitor = Arc::new(Monitor::new(
        prober,
        boot.clone(),
        revision.clone(),
        cfg.probe_interval,
        cfg.wake_timeout,
        cfg.pc_name.clone(),
    ));
    let wol = Arc::new(UdpWol::new(cfg.wol_broadcast, cfg.wol_port));

    let web_addr = SocketAddr::new(cfg.bind_addr, cfg.web_port);
    let grub_addr = SocketAddr::new(cfg.bind_addr, cfg.grub_port);
    let web_listener = TcpListener::bind(web_addr)
        .await
        .with_context(|| format!("can't listen on WEB_PORT {web_addr}"))?;
    let grub_listener = TcpListener::bind(grub_addr)
        .await
        .with_context(|| format!("can't listen on GRUB_PROTOCOL_PORT {grub_addr}"))?;

    tracing::info!(
        version = wake::web::VERSION,
        pc = %cfg.pc_name,
        macs = %cfg.pc_macs.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        pc_ip = ?cfg.pc_ip,
        wol_target = %wol.target(),
        default_boot = %cfg.default_boot,
        "Wake is starting"
    );
    tracing::info!("web UI and API on http://{web_addr}");
    tracing::info!("GRUB endpoint on http://{grub_addr}{}", grub::BOOT_PATH);
    match &cfg.grub_allowed {
        AllowList::Any => tracing::warn!(
            "GRUB endpoint accepts any address; set PC_IP or GRUB_ALLOWED_IPS to restrict it"
        ),
        AllowList::Only(ips) => tracing::info!(?ips, "GRUB endpoint restricted"),
    }
    if cfg.pc_ip.is_none() {
        tracing::warn!("PC_IP isn't set, so Wake can't tell whether the PC is on");
    }
    if cfg.wake_token.is_none() {
        tracing::warn!("WAKE_TOKEN isn't set; anyone on the LAN can wake the PC and pick its OS");
    }

    tokio::spawn(monitor.clone().run());
    tokio::spawn(grub::serve(
        grub_listener,
        GrubContext {
            boot: boot.clone(),
            allowed: cfg.grub_allowed.clone(),
            monitor: Some(monitor.clone()),
        },
    ));

    let app = wake::web::app(AppState {
        cfg: cfg.clone(),
        boot,
        monitor,
        wol,
        revision,
    });
    axum::serve(
        web_listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("web server stopped")?;
    tracing::info!("Wake stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down");
}
