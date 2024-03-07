use futures_channel::mpsc::UnboundedSender;
use std::net::SocketAddr;
use tokio_tungstenite::tungstenite::Message;
use turtle_protocol::User;

#[derive(Clone, Debug)]
pub struct Connection {
    pub addr: SocketAddr,
    pub tx: UnboundedSender<Message>,
    pub user: User,
}
