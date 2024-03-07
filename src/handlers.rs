use crate::connection::Connection;
use crate::db::DbError;
use crate::{CORE_CONN, DB, PEER_MAP};
use futures_channel::mpsc::UnboundedSender;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::time::SystemTime;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{error, info, instrument, warn};
use turtle_protocol::{
    ChannelsInfo, CreateChannel, ForwardMessage, IntoSendable, LoginFail, LoginMessage,
    LoginSuccess, SendChatMessage, SendableId, User, UserId, UserJoined, UsersInfo, WsShell,
};

#[instrument(skip_all)]
pub async fn login(
    incoming: &mut SplitStream<WebSocketStream<TcpStream>>,
    outgoing: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
    tx: &mut UnboundedSender<Message>,
) -> Option<User> {
    loop {
        if let Some(resp) = incoming.next().await {
            let start = SystemTime::now();
            if let Ok(msg) = resp {
                let msg: WsShell = serde_json::from_str(msg.to_text().unwrap())
                    .expect("Failed to deserialize login message");
                let login: LoginMessage =
                    serde_json::from_value(msg.value).expect("should be a login message");
                match DB.login_user(&login).await {
                    Ok(u) => {
                        handle_login_success(&u, tx).await;
                        let elapsed = start.elapsed().unwrap();
                        info!("handled login in {elapsed:?}");
                        return Some(u);
                    }
                    Err(e) => match e.status {
                        StatusCode::INTERNAL_SERVER_ERROR => {
                            send_login_fail(outgoing, "Internal Server Error: failed to fetch user")
                                .await
                        }
                        StatusCode::UNAUTHORIZED => {
                            send_login_fail(outgoing, "wrong password").await
                        }
                        StatusCode::NOT_FOUND => {
                            warn!("{e:?}");
                            match DB.new_user(login).await {
                                Ok(u) => {
                                    handle_login_success(&u, tx).await;
                                    let elapsed = start.elapsed().unwrap();
                                    info!("handled login in {elapsed:?}");
                                    return Some(u);
                                }
                                Err(e) => {
                                    error!("Error while creating new user: {e:?}");
                                    send_login_fail(
                                        outgoing,
                                        "Internal Server Error: failed to create new user",
                                    )
                                    .await;
                                }
                            }
                        }
                        x => warn!("Got unknown status: {x}"),
                    },
                }
            }
        } else {
            info!("closing socket connection");
            match outgoing.close().await {
                Ok(_) => return None,
                Err(_) => return None,
            };
        }
    }
}

#[instrument]
pub async fn create_channel(shell: WsShell, conn: &Connection) -> Result<(), DbError> {
    let create_chan: CreateChannel =
        serde_json::from_value(shell.value).expect("couldn't deserialize Channel added message");
    let chan_added = DB.insert_channel(create_chan, conn.user.id).await?;
    let chan_id = chan_added.channel.id;
    match serde_json::to_string(&chan_added.into_sendable()) {
        Ok(msg_str) => {
            let mut handles = vec![];
            PEER_MAP.lock().await.iter().for_each(|(user_id, conn)| {
                let _ = conn.tx.unbounded_send(Message::Text(msg_str.clone()));
                handles.push(tokio::spawn(DB.add_user_channel(*user_id, chan_id)));
            });
            for handle in handles {
                let _ = handle.await;
            }
        }
        Err(e) => error!("serialization error: {e:?}"),
    }
    Ok(())
}

#[instrument]
pub async fn send_message(msg: SendChatMessage, conn: &Connection) -> anyhow::Result<()> {
    let chat_message = DB.persist_message(msg, conn.user.id).await?;
    let to = chat_message.to;
    let shell = chat_message.clone().into_sendable();
    let msg_str = serde_json::to_string(&chat_message.into_sendable())?;
    match to {
        SendableId::C(chan_id) => match DB.get_channel_users(chan_id).await {
            Ok(users) => {
                let pm = PEER_MAP.lock().await;
                let remote_users: Vec<UserId> = users // users who may be connected to a different server
                    .iter()
                    .filter(|u_id| !pm.contains_key(u_id))
                    .cloned()
                    .collect();
                pm.iter()
                    .filter(|(u_id, _)| users.contains(u_id))
                    .for_each(|(_, c)| {
                        let _ = c.tx.unbounded_send(Message::Text(msg_str.to_string()));
                    });
                let outbound = &ForwardMessage {
                    shell,
                    user_ids: remote_users,
                }
                .into_sendable();
                match serde_json::to_string(outbound) {
                    Ok(fwd_msg) => {
                        let _ = CORE_CONN
                            .lock()
                            .await
                            .unbounded_send(Message::Text(fwd_msg));
                    }
                    Err(e) => error!("failed to forward messsage to core {e:?}"),
                }
                //rabbit_mq::send_to_rmq(shell, remote_users).await?;
            }
            Err(e) => error!("{e:?}"),
        },
        SendableId::U(user_id) => match PEER_MAP.lock().await.get(&user_id) {
            None => {
                let user_ids = vec![user_id];
                let outbound = ForwardMessage { shell, user_ids }.into_sendable();
                match serde_json::to_string(&outbound) {
                    Ok(msg) => {
                        let _ = CORE_CONN.lock().await.unbounded_send(Message::Text(msg));
                        conn.tx.unbounded_send(Message::Text(msg_str))?;
                    }
                    Err(e) => error!("failed to serialize forwarded dm\n{e:?}"),
                }
            }
            Some(c) => {
                c.tx.unbounded_send(Message::Text(msg_str.clone()))?;
                conn.tx.unbounded_send(Message::Text(msg_str))?;
            }
        },
    }
    Ok(())
}

#[instrument]
async fn send_login_fail(
    outgoing: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
    message: &str,
) {
    let login_fail = LoginFail {
        reason: String::from(message),
    };
    let resp = serde_json::to_string(&login_fail.into_sendable()).unwrap();
    outgoing
        .send(Message::Text(resp))
        .await
        .expect("failed to send LoginFail to client")
}

#[instrument]
async fn handle_login_success(user: &User, tx: &mut UnboundedSender<Message>) {
    let login_success = LoginSuccess { id: user.id };
    let resp = serde_json::to_string(&login_success.into_sendable()).unwrap();

    let msg = UserJoined { user: user.clone() };
    let msg_str =
        serde_json::to_string(&msg.into_sendable()).expect("couldn't serialize user added");
    PEER_MAP
        .lock()
        .await
        .iter()
        .filter(|(u_id, _)| u_id != &&user.id)
        .for_each(|(_, c)| {
            let _ = c.tx.unbounded_send(Message::Text(msg_str.clone()));
        });

    let users = DB.get_all_users().await.expect("failed to get user list");
    let channels = DB.get_channels().await.expect("failed to get channel list");

    /*
    let mut handles = Vec::with_capacity(channels.len());
    for chan in channels.iter() {
        handles.push(tokio::spawn(DB.get_chan_msgs(chan.id)));
    }
    for handle in handles {
        match handle.await.expect("couldn't join thread") {
            Ok(messages) => {
                let chan_msgs = ChannelMessages { messages };
                let msg_str = serde_json::to_string(&chan_msgs.into_sendable())
                    .expect("can't serialize shell?");
                let _ = outgoing.send(Message::Text(msg_str)).await;
            }
            Err(e) => {
                info!("{e:?}")
            }
        }
    }
     */

    let users_info = UsersInfo { users };
    let channels_info = ChannelsInfo { channels };
    let users_msg = serde_json::to_string(&users_info.into_sendable()).unwrap();
    let channels_msg = serde_json::to_string(&channels_info.into_sendable()).unwrap();
    let _ = tx.send(Message::Text(resp)).await;
    let _ = tx.send(Message::Text(users_msg)).await;
    let _ = tx.send(Message::Text(channels_msg)).await;
}
