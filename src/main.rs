mod connection;
mod db;
mod handlers;

use crate::connection::Connection;
use crate::db::DBApplication;
use futures::executor;
use futures_channel::mpsc::unbounded;
use futures_channel::mpsc::UnboundedSender;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{future, pin_mut, StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{error, info, instrument, span, warn, Level};
use turtle_protocol::{
    ForwardMessage, IntoSendable, Ping, SendChatMessage, ServerUserJoined, ServerUserLeft,
    ServerUsers, UserId, UserLeft, WsShell,
};

type PeerMap = Arc<Mutex<HashMap<UserId, Connection>>>;

lazy_static! {
    static ref PEER_MAP: PeerMap = PeerMap::new(Mutex::new(HashMap::new()));
    static ref DB: Arc<DBApplication> = Arc::new(
        executor::block_on(DBApplication::new(
            env::var("DATABASE_URL")
                .expect("DATABASE_URL NOT SET")
                .to_string()
                .into(),
        ))
        .expect("failed to establish connection to DB"),
    );
    static ref PING: String =
        serde_json::to_string(&Ping {}.into_sendable()).expect("failed to initialize ping");
    static ref CORE_CONN: Arc<Mutex<UnboundedSender<Message>>> =
        Arc::new(Mutex::new(executor::block_on(connect_to_core())));
}

const ADDR: &str = "0.0.0.0:8889";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let span = span!(Level::INFO, "server");
    let _guard = span.enter();

    DB.init().await?;

    let ping = serde_json::to_string(&Ping {}).expect("couldn't serialize ping message");
    CORE_CONN
        .lock()
        .await
        .unbounded_send(Message::Text(ping))
        .expect("failed to ping core server");

    let try_socket = TcpListener::bind(ADDR).await;
    let listener = try_socket.expect("Failed to bind");
    info!("Listening on: {}", ADDR);

    // Let's spawn the handling of each connection in a separate task.
    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(stream, addr));
    }

    Ok(())
}

#[instrument(skip(raw_stream))]
async fn handle_connection(raw_stream: TcpStream, addr: SocketAddr) {
    let (mut outgoing, mut incoming) = set_up_stream(raw_stream, &addr).await;

    // Insert the write part of this peer to the peer map.
    let (mut tx, rx) = unbounded::<Message>();

    if let Some(user) = handlers::login(&mut incoming, &mut outgoing, &mut tx).await {
        let user_joined_shell = ServerUserJoined { user_id: user.id }.into_sendable();
        let msg = serde_json::to_string(&user_joined_shell)
            .expect("failed to serialize user joined message");
        CORE_CONN
            .lock()
            .await
            .unbounded_send(Message::Text(msg))
            .expect("failed to notify core about new user");

        let conn = Connection {
            addr,
            tx: tx.clone(),
            user,
        };
        PEER_MAP.lock().await.insert(conn.user.id, conn.clone());

        let broadcast = incoming.try_for_each(|msg| {
            let tx = tx.clone();
            let conn = conn.clone();
            async move {
                if let Ok(txt) = msg.to_text() {
                    if let Ok(shell) = serde_json::from_str::<WsShell>(txt) {
                        match shell.type_.as_str() {
                            "CreateChannel" => {
                                let start = SystemTime::now();
                                let _ = handlers::create_channel(shell, &conn).await;
                                let elapsed = start.elapsed().unwrap();
                                info!("handled create channel in {elapsed:?}")
                            }
                            "Ping" => {
                                let ping = Ping {};
                                let resp = serde_json::to_string(&ping.into_sendable())
                                    .expect("couldn't serialize ping message?!?");
                                let _ = tx.unbounded_send(Message::Text(resp));
                            }
                            "SendChatMessage" => {
                                let start = SystemTime::now();
                                let msg: SendChatMessage =
                                    serde_json::from_value(shell.value).unwrap();
                                let res = handlers::send_message(msg, &conn).await;
                                if let Err(e) = res {
                                    info!("got error: {e:?}")
                                }
                                let elapsed = start.elapsed().unwrap();
                                info!("handled send message in {elapsed:?}")
                            }
                            msg_type => info!("Received unexpected message type {msg_type}"),
                        }
                    }
                }

                Ok(())
            }
        });

        let receive = rx.map(Ok).forward(outgoing);

        pin_mut!(broadcast, receive);
        future::select(broadcast, receive).await;

        info!("{} disconnected", conn.user.username);

        // log user out
        DB.set_online_status(conn.user.id, false).await.unwrap();
        let user_left = UserLeft { id: conn.user.id };
        match serde_json::to_string(&user_left.into_sendable()) {
            Ok(msg_str) => {
                let mut pm = PEER_MAP.lock().await;
                let _ = pm.remove(&conn.user.id);
                pm.iter().for_each(|(_, c)| {
                    let _ = c.tx.unbounded_send(Message::Text(msg_str.clone()));
                })
            }
            Err(e) => {
                info!("failed to serialize user left message\n{e:?}");
            }
        }

        let user_left_shell = ServerUserLeft {
            user_id: conn.user.id,
        }
        .into_sendable();
        let msg =
            serde_json::to_string(&user_left_shell).expect("failed to serialize user left message");
        CORE_CONN
            .lock()
            .await
            .unbounded_send(Message::Text(msg))
            .expect("failed to notify core about user leaving");
    }
}

#[instrument(skip(raw_stream))]
async fn set_up_stream(
    raw_stream: TcpStream,
    addr: &SocketAddr,
) -> (
    SplitSink<WebSocketStream<TcpStream>, Message>,
    SplitStream<WebSocketStream<TcpStream>>,
) {
    info!("Incoming TCP connection from: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during the websocket handshake occurred");
    info!("WebSocket connection established: {}", addr);

    // Get sender and receiver for websocket connection
    ws_stream.split()
}

async fn connect_to_core() -> UnboundedSender<Message> {
    // connect to core
    let connect_addr = match env::var("LOCAL_DEV") {
        Ok(_) => "ws://localhost:8000/",
        Err(_) => "wss://tc-core.level9turtles.com",
    }
    .to_string();
    let url = url::Url::parse(&connect_addr).unwrap();
    info!("About to connect_async for {url}");
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");
    info!("WebSocket handshake has been successfully completed");
    let (write, read) = ws_stream.split();

    // make channel for cross-thread communication
    let (tx, rx) = unbounded();

    // spawn a thread to listen to the internal channel and send messages over the websocket
    tokio::spawn(rx.map(Ok).forward(write));
    tokio::spawn(listen_with_reconnect(read));
    tokio::spawn(loop_ping_core());
    tx
}

async fn loop_ping_core() {
    loop {
        let _ = CORE_CONN
            .lock()
            .await
            .unbounded_send(Message::Text(PING.clone()));
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn listen_with_reconnect(read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) {
    listen_to_core(read).await;
    loop {
        // connect to core
        let connect_addr = match env::var("LOCAL_DEV") {
            Ok(_) => "ws://localhost:8000/",
            Err(_) => "wss://tc-core.level9turtles.com",
        }
        .to_string();
        let url = url::Url::parse(&connect_addr).unwrap();
        if let Ok((ws_stream, _)) = connect_async(url).await {
            info!("WebSocket handshake has been successfully completed");
            let (write, read) = ws_stream.split();
            let (tx, rx) = unbounded();
            *CORE_CONN.lock().await = tx.clone();
            tokio::spawn(rx.map(Ok).forward(write));
            let server_users = ServerUsers {
                user_ids: PEER_MAP.lock().await.keys().cloned().collect(),
            }
            .into_sendable();
            if let Ok(msg) = serde_json::to_string(&server_users) {
                let _ = tx.unbounded_send(Message::Text(msg));
                listen_to_core(read).await;
            }
        } else {
            warn!("failed to reconnect");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn listen_to_core(read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) {
    let mut read = read;
    while let Some(Ok(message)) = read.next().await {
        match serde_json::from_str::<WsShell>(
            message.to_text().expect("couldn't convert message to text"),
        ) {
            Ok(shell) => match shell.type_.as_str() {
                "ForwardMessage" => match serde_json::from_value::<ForwardMessage>(shell.value) {
                    Ok(fwd_msg) => match serde_json::to_string(&fwd_msg.shell) {
                        Ok(msg) => {
                            PEER_MAP
                                .lock()
                                .await
                                .iter()
                                .filter(|(u_id, _)| fwd_msg.user_ids.contains(u_id))
                                .for_each(|(_, conn)| {
                                    info!("forwarding message to {}", conn.user.username);
                                    let _ = conn.tx.unbounded_send(Message::Text(msg.clone()));
                                });
                        }
                        Err(e) => error!("failed to serialize forward message\n{e:?}"),
                    },
                    Err(e) => error!("failed to deserialize forward message??\n{e:?}"),
                },
                "Ping" => {}
                x => warn!("received unexpected message type: {x}"),
            },
            Err(e) => error!("failed to deserialize shell from core\n{e:?}"),
        }
    }
    error!("lost connection to core!")
}
