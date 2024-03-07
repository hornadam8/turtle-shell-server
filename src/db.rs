use crate::DB;
use anyhow;
use lazy_static::lazy_static;
use serde::de::StdError;
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::collections::HashMap;
use std::fmt::Display;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tracing::instrument;
use turtle_protocol::{
    Channel, ChannelAdded, ChannelId, ChatMessage, CreateChannel, LoginMessage, MessageId, Role,
    SendChatMessage, SendableId, User, UserId,
};

lazy_static! {
    static ref FLAIR_MAP: HashMap<String, String> = HashMap::from([
        (String::from("ol_rusty_bastard"), String::from("🐢")),
        (String::from("csk"), String::from("🐢")),
        (String::from("realcsk"), String::from("🐢")),
        (String::from("gfunchess"), String::from("🐢")),
        (String::from("cd-stephen"), String::from("🦀")),
    ]);
    static ref ROLE_MAP: HashMap<String, Role> = HashMap::from([
        ("ol_rusty_bastard".to_string(), Role::Op),
        ("csk".to_string(), Role::Op),
        ("realcsk".to_string(), Role::Op),
        ("gfunchess".to_string(), Role::Op),
        ("cd-stephen".to_string(), Role::Op),
        ("Pixel_Outlaw".to_string(), Role::Op)
    ]);
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct DbUser {
    id: i64,
    username: String,
    flair: Option<String>,
    online: bool,
    role: Role,
    password_hash: String,
}

impl From<DbUser> for User {
    fn from(value: DbUser) -> Self {
        Self {
            id: UserId(value.id),
            username: value.username,
            online: value.online,
            flair: value.flair,
            role: value.role,
        }
    }
}

#[derive(Debug)]
struct DbChannel {
    id: i64,
    name: String,
}
impl From<DbChannel> for Channel {
    fn from(value: DbChannel) -> Self {
        Self {
            id: ChannelId(value.id),
            name: value.name,
            has_unread: None,
        }
    }
}

struct DbChatMessage {
    id: i64,
    to_: String,
    from_: String,
    ts: f64,
    content: String,
}

impl TryFrom<DbChatMessage> for ChatMessage {
    type Error = Box<dyn std::error::Error>;

    fn try_from(value: DbChatMessage) -> Result<Self, Self::Error> {
        let id = MessageId(value.id);
        let to = value.to_.parse()?;
        let from = value.from_.parse()?;
        Ok(Self {
            id,
            to,
            from,
            ts: value.ts,
            content: value.content,
        })
    }
}

#[derive(Debug)]
pub struct DbError {
    pub status: StatusCode,
    pub message: String,
}

impl Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl StdError for DbError {}

#[derive(Debug)]
pub struct DBApplication {
    pool: PgPool,
}

impl DBApplication {
    pub async fn new(config: String) -> anyhow::Result<DBApplication> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&config)
            .await?;
        let db = DBApplication { pool };
        Ok(db)
    }

    #[instrument]
    pub async fn init(&self) -> anyhow::Result<()> {
        self.init_users().await?;
        self.init_channels().await?;
        self.init_user_channels().await?;
        self.init_messages().await?;
        self.init_general_channel().await?;
        Ok(())
    }

    #[instrument]
    async fn init_users(&self) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id BIGSERIAL PRIMARY KEY NOT NULL,
                username VARCHAR(255) UNIQUE NOT NULL,
                password_hash VARCHAR(255) NOT NULL,
                online BOOL NOT NULL,
                flair VARCHAR(255),
                role VARCHAR(255) NOT NULL
            );
        "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(())
    }

    #[instrument]
    async fn init_channels(&self) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
                CREATE TABLE IF NOT EXISTS channels (
                    id BIGSERIAL PRIMARY KEY NOT NULL,
                    name VARCHAR(255) UNIQUE NOT NULL
                );
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(())
    }

    #[instrument]
    async fn init_user_channels(&self) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
                CREATE TABLE IF NOT EXISTS user_channels (
                    id BIGSERIAL PRIMARY KEY NOT NULL,
                    user_id BIGSERIAL NOT NULL,
                    channel_id BIGSERIAL NOT NULL,
                    FOREIGN KEY (user_id) REFERENCES users(id),
                    FOREIGN KEY (channel_id) REFERENCES channels(id)
                );
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(())
    }

    #[instrument]
    async fn init_messages(&self) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
                CREATE TABLE IF NOT EXISTS messages (
                    id BIGSERIAL PRIMARY KEY NOT NULL,
                    to_ VARCHAR(255) NOT NULL,
                    from_ VARCHAR(255) NOT NULL,
                    ts FLOAT NOT NULL,
                    content TEXT NOT NULL
                );
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(())
    }

    #[instrument]
    async fn init_general_channel(&self) -> anyhow::Result<()> {
        if sqlx::query!("SELECT id FROM channels WHERE name = $1", "general")
            .fetch_all(&self.pool)
            .await?
            .is_empty()
        {
            sqlx::query!("INSERT INTO channels (name) VALUES ($1)", "general",)
                .fetch_all(&self.pool)
                .await?;
        }
        Ok(())
    }

    #[instrument]
    pub async fn find_user_by_username(&self, username: &str) -> Result<User, DbError> {
        let db_user = sqlx::query_as!(DbUser, "SELECT * FROM users WHERE username = $1", username)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to query db\n{e:?}"),
            })?;
        Ok(db_user.into())
    }

    #[instrument]
    pub async fn login_user(&self, login: &LoginMessage) -> Result<User, DbError> {
        let db_user: DbUser = sqlx::query_as!(
            DbUser,
            "SELECT * FROM users WHERE username = $1",
            login.username
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::NOT_FOUND,
            message: format!("no user found for username: {}\n{e:?}", login.username),
        })?;
        let verified =
            bcrypt::verify(login.password.as_str(), &db_user.password_hash).map_err(|e| {
                DbError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("bcrypt failed to compare the password to its hash\n{e:?}"),
                }
            })?;
        if verified {
            let mut user: User = db_user.into();
            user.online = true;
            self.set_online_status(user.id, true).await?;
            Ok(user)
        } else {
            Err(DbError {
                status: StatusCode::UNAUTHORIZED,
                message: String::from("Failed to authenticate user"),
            })
        }
    }

    #[instrument]
    pub async fn new_user(&self, login: LoginMessage) -> Result<User, DbError> {
        let flair = FLAIR_MAP.get(&login.username).cloned();
        let role = ROLE_MAP
            .get(&login.username)
            .cloned()
            .unwrap_or(Role::Member);
        let id = sqlx::query!(
            "INSERT INTO users (username, password_hash, online, flair, role) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            login.username,
            bcrypt::hash(login.password, 4).unwrap(),
            true,
            flair,
            role.to_string()
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to insert new user into the database\n{e:?}"),
        })?
        .id;
        self.add_user_channel_by_name(UserId(id), "general").await?;
        let id = UserId(id);
        Ok(User {
            id,
            username: String::from(login.username),
            online: true,
            flair,
            role: Role::Member,
        })
    }

    #[instrument]
    pub async fn set_online_status(&self, user_id: UserId, status: bool) -> Result<(), DbError> {
        sqlx::query!(
            "UPDATE users SET online = $1 WHERE id = $2",
            status,
            user_id.0
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to update online status\n{e:?}"),
        })?;
        Ok(())
    }

    #[instrument]
    pub async fn get_other_users(&self, user_id: UserId) -> Result<Vec<User>, DbError> {
        let db_users = sqlx::query_as!(DbUser, "SELECT * FROM users WHERE id != $1", user_id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to get users from db\n{e:?}"),
            })?;
        let users = db_users.into_iter().map(|u| u.into()).collect();
        Ok(users)
    }

    #[instrument]
    pub async fn get_channels(&self) -> Result<Vec<Channel>, DbError> {
        let db_channels = sqlx::query_as!(DbChannel, "SELECT * FROM channels")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to lookup channels\n{e:?}"),
            })?;
        let channels = db_channels.into_iter().map(|c| c.into()).collect();
        Ok(channels)
    }

    #[instrument]
    pub async fn add_user_channel_by_name(
        &self,
        user_id: UserId,
        chan_name: &str,
    ) -> Result<(), DbError> {
        let chan_id = sqlx::query!("SELECT id FROM channels WHERE name = $1", chan_name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("Failed to fetch channel\n{e:?}"),
            })?
            .id;
        self.add_user_channel(user_id, ChannelId(chan_id)).await?;
        Ok(())
    }

    #[instrument]
    pub async fn add_user_channel(
        &self,
        user_id: UserId,
        chan_id: ChannelId,
    ) -> Result<(), DbError> {
        sqlx::query!(
            "INSERT INTO user_channels (user_id, channel_id) VALUES ($1, $2)",
            user_id.0,
            chan_id.0
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("Failed to insert user channel\n{e:?}"),
        })?;

        let mut users = DB.get_channel_users(chan_id).await?;
        users.push(user_id);

        Ok(())
    }

    #[instrument]
    pub async fn persist_message(
        &self,
        msg: SendChatMessage,
        sender_id: UserId,
    ) -> Result<ChatMessage, DbError> {
        let (recp_user_id, recp_chan_id) = match msg.to {
            SendableId::C(chan_id) => (None, Some(chan_id.0)),
            SendableId::U(user_id) => (Some(user_id.0), None),
        };
        let ts = chrono::Utc::now().timestamp_millis() as f64;
        let id = sqlx::query!(
            "INSERT INTO messages (to_, from_, ts, content) VALUES ($1, $2, $3, $4) RETURNING id",
            msg.to.to_string(),
            sender_id.to_string(),
            ts,
            msg.content
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to insert message to db\n{e:?}"),
        })?
        .id;
        let send_id: SendableId = if let Some(id) = recp_user_id {
            SendableId::U(UserId(id))
        } else {
            SendableId::C(ChannelId(recp_chan_id.unwrap()))
        };

        let chat_message = ChatMessage {
            id: MessageId(id),
            to: send_id,
            from: sender_id,
            ts,
            content: msg.content,
        };
        Ok(chat_message)
    }

    #[instrument]
    pub async fn get_channel_users(&self, chan_id: ChannelId) -> Result<Vec<UserId>, DbError> {
        let user_ids: Vec<UserId> = sqlx::query!(
            r#"
            SELECT users.id 
            FROM users
            JOIN user_channels ON user_channels.user_id=users.id AND user_channels.channel_id = $1
            "#,
            chan_id.0
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to find channel users\n{e:?}"),
        })?
        .iter()
        .map(|rec| UserId(rec.id))
        .collect();
        Ok(user_ids)
    }

    #[instrument]
    pub async fn get_all_users(&self) -> Result<Vec<User>, DbError> {
        let users: Vec<User> = sqlx::query_as!(DbUser, "SELECT * FROM users")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to retrieve users\n{e:?}"),
            })?
            .into_iter()
            .map(|db_user| db_user.into())
            .collect();
        Ok(users)
    }

    #[instrument]
    pub async fn get_chan_msgs(&self, chan_id: ChannelId) -> Result<Vec<ChatMessage>, DbError> {
        let chat_msgs: Vec<ChatMessage> = sqlx::query_as!(
            DbChatMessage,
            "SELECT * FROM messages WHERE to_ = $1",
            chan_id.to_string()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to retrieve messages\n{e:?}"),
        })?
        .into_iter()
        .map(|db_chat_msg| db_chat_msg.try_into())
        .filter(|maybe_chat_msg| maybe_chat_msg.is_ok())
        .map(|chat_msg| chat_msg.unwrap())
        .collect();
        Ok(chat_msgs)
    }

    #[instrument]
    pub async fn insert_channel(
        &self,
        chan: CreateChannel,
        user_id: UserId,
    ) -> Result<ChannelAdded, DbError> {
        let chan: Channel = sqlx::query_as!(
            DbChannel,
            "INSERT INTO channels (name) VALUES ($1) RETURNING *",
            chan.name
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to insert channel\n{e:?}"),
        })?
        .into();
        let chan_added = ChannelAdded {
            channel: chan,
            created_by: user_id,
            ts: chrono::Utc::now().timestamp_millis() as f64,
        };
        Ok(chan_added)
    }
}
