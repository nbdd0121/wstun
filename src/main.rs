use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use http::Uri;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tun::{Tun, TunBuilder};
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

#[derive(clap::Args)]
struct ConnectOpt {
    url: String,
    #[arg(long)]
    credential: Option<String>,
    #[arg(long)]
    tun: String,
    #[arg(long)]
    once: bool,
}

#[derive(clap::Args)]
struct BindOpt {
    address: String,
    #[arg(long)]
    tun: String,
    #[arg(long)]
    once: bool,
}

#[derive(Parser)]
enum Opt {
    Connect(ConnectOpt),
    Bind(BindOpt),
}

async fn handle_connection<S>(stream: WebSocketStream<S>, tun: &Tun) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut send, mut recv) = stream.split();

    let recv = async move {
        while let Some(packet) = recv.next().await {
            let packet = packet?;
            log::debug!("Received: {}", packet);

            match packet {
                Message::Binary(content) => {
                    tun.send(&content).await?;
                }
                _ => {
                    log::info!("Received non-binary message: {}", packet);
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    let send = async move {
        loop {
            let mut packet = vec![0; 4096];
            let packet_len = tun.recv(&mut packet).await?;
            packet.truncate(packet_len);

            send.feed(Message::Binary(packet)).await?;
            send.flush().await?;
        }
        // This is a type inference guide.
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    tokio::try_join!(recv, send)?;

    Ok(())
}

async fn connect(opt: &ConnectOpt) -> Result<()> {
    let tun = TunBuilder::new().name(&opt.tun).up().try_build()?;

    let uri: Uri = opt.url.parse()?;

    loop {
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        let ws_stream = loop {
            // This is to replicate what's being done by tungstenite.
            // We have to build our own `http::Request` so that we can add authorization header.
            let mut builder = http::Request::builder()
                .header("Host", uri.authority().context("no host in url")?.as_str())
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header("Sec-WebSocket-Version", "13")
                .header(
                    "Sec-WebSocket-Key",
                    tungstenite::handshake::client::generate_key(),
                )
                .uri(&uri);
            if let Some(credential) = opt.credential.as_deref() {
                builder = builder.header(
                    "Authorization",
                    format!(
                        "Basic {}",
                        data_encoding::BASE64.encode(credential.as_bytes())
                    ),
                );
            }

            match tokio_tungstenite::connect_async(builder.body(())?).await {
                Ok((ws, _)) => {
                    break ws;
                }
                Err(err) => {
                    log::error!("Connection failed: {}", err);

                    if opt.once {
                        Err(err)?;
                    }

                    tokio::time::sleep(backoff).await;
                    backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
                }
            }
        };
        log::info!("Connection established");

        match handle_connection(ws_stream, &tun).await {
            Ok(()) => {
                log::info!("Connection closed");
                break;
            }
            Err(e) => {
                log::error!("Error: {}", e);
            }
        }

        if opt.once {
            break;
        }
    }

    Ok(())
}

async fn bind(opt: &BindOpt) -> Result<()> {
    let tun = Arc::new(TunBuilder::new().name(&opt.tun).up().try_build()?);
    let listener = tokio::net::TcpListener::bind(&opt.address).await?;
    log::info!("Listening on: {}", opt.address);

    let mut conn: Option<tokio::task::JoinHandle<_>> = None;

    loop {
        let (stream, peer) = listener.accept().await?;
        log::info!("Peer address: {}", peer);

        let ws_stream = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(err) => {
                log::error!("Connection failed: {err}");

                if opt.once {
                    break;
                }

                continue;
            }
        };
        log::info!("Connection established");

        if let Some(conn) = conn {
            conn.abort();
        }

        let tun_clone = tun.clone();
        conn = Some(tokio::spawn(async move {
            match handle_connection(ws_stream, &tun_clone).await {
                Ok(()) => {
                    log::info!("Connection closed");
                }
                Err(err) => {
                    log::error!("Error: {}", err);
                }
            }
        }));

        if opt.once {
            break;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let opt = Opt::parse();
    match opt {
        Opt::Connect(opt) => {
            connect(&opt).await?;
        }
        Opt::Bind(opt) => {
            bind(&opt).await?;
        }
    }
    Ok(())
}
