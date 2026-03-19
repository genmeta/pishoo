use std::{future::Future, sync::Mutex};

use h3x::{
    quic,
    remoc::quic::{ConnectionClient, serve_quic_connection},
};
use tokio::task::JoinSet;
use tracing::Instrument;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteErrorMessage(String);

impl RemoteErrorMessage {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for RemoteErrorMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ListenerHandle {
    client: ListenerClient,
}

impl ListenerHandle {
    pub fn new(client: ListenerClient) -> Self {
        Self { client }
    }

    pub async fn accept(&self) -> Result<ConnectionClient, ListenError> {
        self.client.accept().await
    }

    pub async fn shutdown(&self) -> Result<(), ListenError> {
        self.client.shutdown().await
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConnectorHandle {
    client: ConnectorClient,
}

impl ConnectorHandle {
    pub fn new(client: ConnectorClient) -> Self {
        Self { client }
    }

    pub async fn connect(&self, server: String) -> Result<ConnectionClient, ConnectError> {
        self.client.connect(server).await
    }
}

#[derive(Debug, snafu::Snafu, Clone, serde::Serialize, serde::Deserialize)]
#[snafu(module)]
pub enum ListenError {
    #[snafu(display("remote listener error: {message}"))]
    Remote { message: RemoteErrorMessage },
    #[snafu(transparent)]
    Call { source: remoc::rtc::CallError },
}

#[derive(Debug, snafu::Snafu, Clone, serde::Serialize, serde::Deserialize)]
#[snafu(module)]
pub enum ConnectError {
    #[snafu(display("remote connector error: {message}"))]
    Remote { message: RemoteErrorMessage },
    #[snafu(transparent)]
    Call { source: remoc::rtc::CallError },
}

#[remoc::rtc::remote]
pub trait Listener: Send + Sync {
    async fn accept(&self) -> Result<ConnectionClient, ListenError>;
    async fn shutdown(&self) -> Result<(), ListenError>;
}

#[remoc::rtc::remote]
pub trait Connector: Send + Sync {
    async fn connect(&self, server: String) -> Result<ConnectionClient, ConnectError>;
}

pub struct ServedListener<L> {
    listener: L,
    tasks: TaskSet,
}

impl<L> ServedListener<L> {
    pub fn new(listener: L) -> Self {
        Self {
            listener,
            tasks: TaskSet::new(),
        }
    }
}

impl<L> Listener for ServedListener<L>
where
    L: quic::Listen + Send + Sync + 'static,
    L::Connection: 'static,
    <L::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
    <L::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
{
    async fn accept(&self) -> Result<ConnectionClient, ListenError> {
        let connection = self
            .listener
            .accept()
            .await
            .map_err(|source| ListenError::Remote {
                message: RemoteErrorMessage::new(source.to_string()),
            })?;
        let (client, fut) = serve_quic_connection(connection);
        self.tasks.spawn(fut);
        Ok(client)
    }

    async fn shutdown(&self) -> Result<(), ListenError> {
        self.listener
            .shutdown()
            .await
            .map_err(|source| ListenError::Remote {
                message: RemoteErrorMessage::new(source.to_string()),
            })
    }
}

pub struct ServedConnector<C> {
    connector: C,
    tasks: TaskSet,
}

impl<C> ServedConnector<C> {
    pub fn new(connector: C) -> Self {
        Self {
            connector,
            tasks: TaskSet::new(),
        }
    }
}

impl<C> Connector for ServedConnector<C>
where
    C: quic::Connect + Send + Sync + 'static,
    C::Connection: 'static,
    <C::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
    <C::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
{
    async fn connect(&self, server: String) -> Result<ConnectionClient, ConnectError> {
        let authority = http::uri::Authority::try_from(server).map_err(|source| {
            ConnectError::Remote {
                message: RemoteErrorMessage::new(source.to_string()),
            }
        })?;
        let connection =
            self.connector
                .connect(&authority)
                .await
                .map_err(|source| ConnectError::Remote {
                    message: RemoteErrorMessage::new(source.to_string()),
                })?;
        let (client, fut) = serve_quic_connection(connection);
        self.tasks.spawn(fut);
        Ok(client)
    }
}

struct TaskSet {
    inner: Mutex<JoinSet<()>>,
}

impl TaskSet {
    fn new() -> Self {
        Self {
            inner: Mutex::new(JoinSet::new()),
        }
    }

    fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        let mut set = self.inner.lock().unwrap();
        while set.try_join_next().is_some() {}
        set.spawn(fut.in_current_span());
    }
}
