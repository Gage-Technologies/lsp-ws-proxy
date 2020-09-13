use std::{process::Stdio, str::FromStr, time::Duration};

use argh::FromArgs;
use async_fs::File;
use async_io::Timer;
use async_net::{SocketAddr, TcpListener};
use async_process::{Child, Command};
use async_tungstenite::{accept_async, tungstenite as ws};
use futures_util::{
    future::{select, Either},
    AsyncWriteExt, SinkExt, StreamExt,
};

mod client;
mod lsp;

// TODO Remap Document URIs

#[derive(FromArgs)]
// Using block doc comments so that `argh` preserves newlines in help output.
// We need to also write block doc comments without leading space.
/**
Start WebSocket proxy for the LSP Server.
Anything after the option delimiter is used to start the server.

Examples:
  lsp-ws-proxy -- langserver
  lsp-ws-proxy -- langserver --stdio
  lsp-ws-proxy --listen 8888 -- langserver --stdio
  lsp-ws-proxy --listen 0.0.0.0:8888 -- langserver --stdio
  lsp-ws-proxy -l 8888 -- langserver --stdio
*/
struct Options {
    /// address or localhost's port to listen on (default: 9999)
    #[argh(
        option,
        short = 'l',
        default = "String::from(\"127.0.0.1:9999\")",
        from_str_fn(parse_listen)
    )]
    listen: String,
    // TODO Using seconds for now for simplicity. Maybe accept duration strings like `1h` instead.
    /// inactivity timeout in seconds
    #[argh(option, short = 't', default = "0")]
    timeout: u64,
    /// write text document to disk on save
    #[argh(switch, short = 's')]
    sync: bool,
    /// show version and exit
    #[argh(switch, short = 'v')]
    version: bool,
}

// Large enough value used to disable inactivity timeout.
const NO_TIMEOUT: u64 = 60 * 60 * 24 * 30 * 12;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TODO Accept option for log level
    env_logger::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (opts, command) = get_opts_and_command();
    let timeout = if opts.timeout != 0 {
        Duration::from_secs(opts.timeout)
    } else {
        Duration::from_secs(NO_TIMEOUT)
    };

    smol::block_on(async {
        let listener = TcpListener::bind(&opts.listen)
            .await
            .expect("Failed to bind");
        log::info!("Listening on {}", listener.local_addr()?);

        // Only accept single connection.
        let (stream, _) = listener.accept().await?;
        let stream = accept_async(stream)
            .await
            .expect("Error during the websocket handshake occurred");
        log::info!("Connection Established");

        let mut server = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut server_send = lsp::framed::writer(server.stdin.take().unwrap());
        let mut server_recv = lsp::framed::reader(server.stdout.take().unwrap());
        let (mut client_send, client_recv) = stream.split();
        let mut client_recv = client_recv
            .filter_map(client::filter_map_ws_message)
            .boxed_local();

        let mut client_msg = client_recv.next();
        let mut server_msg = server_recv.next();
        // Timer for inactivity timeout that resets whenever a message comes in.
        let mut timer = Timer::after(timeout);
        loop {
            match select(select(client_msg, server_msg), timer).await {
                // From Client
                Either::Left((Either::Left((from_client, p_server_msg)), p_timer)) => {
                    match from_client {
                        // Valid LSP message
                        Some(Ok(client::Message::Message(msg))) => {
                            // TODO remap document uri
                            inspect_message_from_client(&msg);
                            if opts.sync {
                                maybe_write_text_document(&msg).await?;
                            }
                            server_send.send(serde_json::to_string(&msg)?).await?;
                        }

                        // Invalid JSON body
                        Some(Ok(client::Message::Invalid(text))) => {
                            log::debug!("Received invalid JSON: {}", text);
                            // Just forward it to the server as is.
                            server_send.send(text).await?;
                        }

                        // Close message
                        Some(Ok(client::Message::Close(_))) => {
                            // The connection will terminate when None is received.
                            log::info!("Received Close Message");
                        }

                        // WebSocket Error
                        Some(Err(err)) => {
                            log::error!("{}", err);
                        }

                        // Connection closed
                        None => {
                            log::info!("Connection Closed");
                            break;
                        }
                    }

                    client_msg = client_recv.next();
                    server_msg = p_server_msg;
                    timer = p_timer;
                    timer.set_after(timeout);
                }

                // From Server
                Either::Left((Either::Right((from_server, p_client_msg)), p_timer)) => {
                    match from_server {
                        // Serialized LSP Message
                        Some(Ok(text)) => {
                            inspect_message_from_server(&text);
                            // TODO transform the message
                            client_send.send(ws::Message::text(text)).await?;
                        }

                        // Codec Error
                        Some(Err(err)) => {
                            log::error!("{}", err);
                        }

                        // Server exited
                        None => {
                            log::error!("Server process exited unexpectedly");
                            client_send.send(ws::Message::Close(None)).await?;
                            break;
                        }
                    }

                    client_msg = p_client_msg;
                    server_msg = server_recv.next();
                    timer = p_timer;
                    timer.set_after(timeout);
                }

                // Inactivity Timeout
                Either::Right(_) => {
                    log::info!("Inactivity timeout reached. Closing");
                    client_send.send(ws::Message::Close(None)).await?;
                    break;
                }
            }
        }

        ensure_server_exited(&mut server).await?;
        Ok(())
    })
}

fn get_opts_and_command() -> (Options, Vec<String>) {
    let strings: Vec<String> = std::env::args().collect();
    let splitted: Vec<&[String]> = strings.splitn(2, |s| *s == "--").collect();
    let strs: Vec<&str> = splitted[0].iter().map(|s| s.as_str()).collect();

    // Parse options or show help and exit.
    let opts = Options::from_args(&[strs[0]], &strs[1..]).unwrap_or_else(|early_exit| {
        // show generated help message
        println!("{}", early_exit.output);
        std::process::exit(match early_exit.status {
            Ok(()) => 0,
            Err(()) => 1,
        })
    });

    if opts.version {
        println!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    if splitted.len() != 2 {
        panic!("Command to start the server is required. See --help for examples.");
    }

    (opts, splitted[1].to_vec())
}

async fn maybe_write_text_document(m: &lsp::Message) -> Result<(), std::io::Error> {
    if let lsp::Message::Notification(lsp::Notification::DidSave { params }) = m {
        if let Some(text) = &params.text {
            let uri = &params.text_document.uri;
            if uri.scheme() == "file" {
                if let Ok(path) = uri.to_file_path() {
                    log::debug!("writing to {:?}", path);
                    let mut file = File::create(&path).await?;
                    file.write_all(text.as_bytes()).await?;
                    file.flush().await?;
                }
            }
        }
    }
    Ok(())
}

fn inspect_message_from_client(msg: &lsp::Message) {
    match msg {
        lsp::Message::Notification(notification) => {
            log::debug!("--> Notification: {:?}", notification);
        }

        lsp::Message::Request(request) => {
            log::debug!("--> Request: {:?}", request);
        }

        lsp::Message::Response(response) => {
            log::debug!("--> Response: {:?}", response);
        }

        lsp::Message::Unknown(unknown) => {
            log::debug!("--> Unknown: {:?}", unknown);
        }
    }
}

fn inspect_message_from_server(text: &str) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }

    match lsp::Message::from_str(text) {
        Ok(lsp::Message::Notification(notification)) => {
            log::debug!("<-- Notification: {:?}", notification);
        }

        Ok(lsp::Message::Response(response)) => {
            log::debug!("<-- Response: {:?}", response);
        }

        Ok(lsp::Message::Request(request)) => {
            log::debug!("<-- Request: {:?}", request);
        }

        Ok(lsp::Message::Unknown(unknown)) => {
            log::debug!("<-- Unknown: {:?}", unknown);
        }

        Err(err) => {
            log::error!("<-- Invalid: {:?}", err);
        }
    }
}

async fn ensure_server_exited(server: &mut Child) -> Result<(), std::io::Error> {
    match server.try_status()? {
        Some(status) => {
            log::info!("Language Server exited");
            log::info!("Status: {}", status);
            Ok(())
        }

        None => {
            log::info!("Language Server is still alive. Waiting 3s before killing.");
            let status = Box::pin(server.status());
            let timeout = Timer::after(Duration::from_secs(3));
            match select(status, timeout).await {
                Either::Left((Ok(status), _)) => {
                    log::info!("Language Server exited");
                    log::info!("Status: {}", status);
                    Ok(())
                }
                Either::Left((Err(err), _)) => Err(err),

                Either::Right(_) => {
                    log::info!("Killing Language Server...");
                    match server.kill() {
                        Ok(_) => {
                            log::info!("Killed Language Server");
                            log::info!("Status: {}", server.status().await?);
                            Ok(())
                        }

                        Err(err) => match err.kind() {
                            // The process had already exited
                            std::io::ErrorKind::InvalidInput => {
                                log::info!("Language Server had already exited");
                                log::info!("Status: {}", server.status().await?);
                                Ok(())
                            }

                            _ => {
                                log::error!("Failed to kill Language Server: {}", err);
                                Err(err)
                            }
                        },
                    }
                }
            }
        }
    }
}

fn parse_listen(value: &str) -> Result<String, String> {
    // If a number is given, treat it as a localhost's port number
    if value.chars().all(|c| c.is_ascii_digit()) {
        return Ok(format!("127.0.0.1:{}", value));
    }

    match value.parse::<SocketAddr>() {
        Ok(_) => Ok(String::from(value)),
        Err(_) => Err(format!("{} cannot be parsed as SocketAddr", value)),
    }
}
