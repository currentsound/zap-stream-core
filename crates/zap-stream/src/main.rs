use crate::api::Api;
use crate::http::HttpServer;
use crate::overseer::ZapStreamOverseer;
use crate::settings::Settings;
use anyhow::Result;
use clap::Parser;
use config::Config;
use ffmpeg_rs_raw::ffmpeg_sys_the_third::{av_log_set_callback, av_version_info};
use ffmpeg_rs_raw::{av_log_redirect, rstr};
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use log::{error, info};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::sleep;
use zap_stream_core::listen::try_create_listener;
use zap_stream_core::overseer::Overseer;

mod api;
mod auth;
mod http;
mod overseer;
mod settings;
mod stream_manager;
mod viewer;
mod websocket_metrics;

#[derive(Parser, Debug)]
struct Args {}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    info!("Starting zap-stream");

    let _args = Args::parse();

    unsafe {
        av_log_set_callback(Some(av_log_redirect));
        info!("FFMPEG version={}", rstr!(av_version_info()));
    }

    let builder = Config::builder()
        .add_source(config::File::with_name("config.yaml"))
        .add_source(config::Environment::with_prefix("APP"))
        .build()?;

    let settings: Settings = builder.try_deserialize()?;
    let (overseer, api) = {
        let overseer = ZapStreamOverseer::from_settings(&settings).await?;
        let arc = Arc::new(overseer.clone());
        let api = Api::new(arc.clone(), settings.clone());
        (arc as Arc<dyn Overseer>, api)
    };
    // Create ingress listeners
    let mut tasks = vec![];
    for e in &settings.endpoints {
        match try_create_listener(e, &settings.output_dir, &overseer) {
            Ok(l) => tasks.push(l),
            Err(e) => error!("{}", e),
        }
    }

    let http_addr: SocketAddr = settings.listen_http.parse()?;

    // HTTP server
    let server = HttpServer::new(PathBuf::from(settings.output_dir), api);
    tasks.push(tokio::spawn(async move {
        let listener = TcpListener::bind(&http_addr).await?;

        loop {
            let (socket, _) = listener.accept().await?;
            let io = TokioIo::new(socket);
            let server = server.clone();
            let mut b = http1::Builder::new();
            b.keep_alive(true);

            tokio::spawn(async move {
                if let Err(e) = b.serve_connection(io, server).with_upgrades().await {
                    error!("Failed to handle request: {}", e);
                }
            });
        }
    }));

    // Background worker to check streams
    let bg = overseer.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            if let Err(e) = bg.check_streams().await {
                error!("{}", e);
            }
            sleep(Duration::from_secs(10)).await;
        }
    }));

    // Join tasks and get errors
    for handle in tasks {
        if let Err(e) = handle.await? {
            error!("{e}");
        }
    }
    info!("Server closed");
    Ok(())
}
