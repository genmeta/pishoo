#![allow(unused)]
use std::{
    fmt::Debug,
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::SeqCst},
    },
    task::{Context, Poll, ready},
};

use bytes::Bytes;
use dashmap::{DashMap, Entry};
use derive_more::Display;
use futures::{
    FutureExt, Sink, SinkExt, Stream, StreamExt, TryStream, TryStreamExt, channel::mpsc,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io;
use tokio_util::{codec, io::StreamReader};
use tracing::Instrument;

use super::cbor_codec;

#[derive(Debug, Display, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Token(u64);

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Client,
    Server,
}

impl Token {
    pub fn new(role: Role, seq: u64) -> Self {
        let mut token = seq << 1;
        match role {
            Role::Client => token |= 0b01,
            Role::Server => token |= 0b00,
        }
        Token(token)
    }

    pub fn seq(&self) -> u64 {
        self.0 >> 1
    }

    pub fn role(&self) -> Role {
        if self.0 & 0b01 == 0 {
            Role::Server
        } else {
            Role::Client
        }
    }

    pub fn into_inner(self) -> u64 {
        self.0
    }

    pub fn next(&self) -> Self {
        Token(self.0 + 2)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ChannelMessage {
    Open { token: Token, open: OpenChannel },
    Data { token: Token, data: Bytes },
    Close { token: Token },
}

impl ChannelMessage {
    pub fn token(&self) -> Token {
        match self {
            ChannelMessage::Open { token, .. } => *token,
            ChannelMessage::Data { token, .. } => *token,
            ChannelMessage::Close { token } => *token,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum OpenChannel {
    Auth { username: String },
    Shell { pseudo: bool },
    Exec { pseudo: bool, command: String },
    Socks {},
    Heartbeat {},
}

pub struct Mux {
    token_gen: AtomicU64,
    channels: DashMap<Token, mpsc::Sender<io::Result<Bytes>>>,
    message_sender: mpsc::Sender<ChannelMessage>,
}

impl Debug for Mux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mux")
            .field("token_gen", &self.token_gen)
            .field("channels", &self.channels)
            .field("message_sender", &"...")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Peer has the same role with local when routing for {0}")]
    SameRole(Token),
    #[error("Channel {0} already be opened")]
    ChannelAlreadyOpen(Token),
    #[error("Failed to send open for {0}: {1}")]
    SendOpen(Token, io::Error),
}

impl Mux {
    pub fn new<St, StE, Si>(
        role: Role,
        mut stream: St,
        mut sink: Si,
    ) -> (Arc<Self>, mpsc::Receiver<Result<NewChannel, super::Error>>)
    where
        St: Stream<Item = Result<ChannelMessage, StE>> + Send + Unpin + 'static,
        StE: Into<super::Error> + Send + 'static,
        Si: Sink<ChannelMessage, Error: Debug> + Send + Unpin + 'static,
    {
        let (message_sender, mut pending_messages) = mpsc::channel::<ChannelMessage>(8);
        let (mut incoming_forwarder, mut incomings) = mpsc::channel(8);

        let this = Arc::new(Mux {
            token_gen: AtomicU64::new(Token::new(role, 0).into_inner()),
            channels: DashMap::new(),
            message_sender,
        });

        let mux = this.clone();
        let task = async move {
            let recv_messages = async {
                while let Some(item) = stream.next().await {
                    let item = match item {
                        Ok(item) => mux.receive(item).await.map_err(super::Error::from),
                        Err(error) => Err(error.into()),
                    };

                    let is_err = item.is_err();
                    if let Some(item) = item.transpose() {
                        tracing::trace!(target: "mux", ?item, "Forward incoming message");
                        _ = incoming_forwarder.send(item).await;
                    }
                    if is_err {
                        return;
                    }
                }
                tracing::debug!(target: "mux", "Incoming stream closed");
            };
            let send_messages = async {
                while let Some(message) = pending_messages.next().await {
                    tracing::trace!(target: "mux", ?message, "Send message");
                    if let Err(error) = sink.send(message).await {
                        tracing::warn!(target: "mux", ?error, "Failed to send message");
                        return;
                    }
                }
            };
            tokio::select! {
                _ = recv_messages => {},
                _ = send_messages => {},
            }
            _ = sink.close().await;
            tracing::debug!(target: "mux", "Sink closed");
        };

        tokio::spawn(task.in_current_span());
        (this, incomings)
    }

    fn token(&self) -> Token {
        Token(self.token_gen.load(SeqCst))
    }

    fn next_token(&self) -> Token {
        let token = self.token_gen.fetch_add(2, SeqCst);
        Token(token)
    }

    fn receive_from<S, E>(
        mux: Arc<Self>,
        messages: S,
    ) -> impl TryStream<Ok = NewChannel, Error = super::Error> + Unpin + use<S, E>
    where
        S: Stream<Item = Result<ChannelMessage, E>> + Send + Unpin + 'static,
        E: Into<super::Error> + Send + 'static,
    {
        let receive = move |message: Result<ChannelMessage, E>| {
            let mux = mux.clone();
            async move {
                match message {
                    Ok(message) => mux.receive(message).await.map_err(Into::into).transpose(),
                    Err(e) => Some(Err(e.into())),
                }
            }
        };
        messages.filter_map(receive).boxed()
    }

    async fn receive(
        self: &Arc<Self>,
        message: ChannelMessage,
    ) -> Result<Option<NewChannel>, Error> {
        tracing::trace!(target: "mux", ?message, "Received message");
        match message {
            ChannelMessage::Open { token, open } => {
                if token.role() == self.token().role() {
                    return Err(Error::SameRole(token));
                }
                let (sender, recver) = mpsc::channel(8);
                let entry = self.channels.entry(token);
                if let Entry::Occupied(..) = &entry {
                    return Err(Error::ChannelAlreadyOpen(token));
                }
                entry.insert(sender);
                let channel = NewChannel {
                    token,
                    mux: self.clone(),
                    recver,
                    open,
                };
                Ok(Some(channel))
            }
            ChannelMessage::Data { token, data } => {
                let channel = self.channels.entry(token);
                if let Entry::Occupied(mut channel) = channel {
                    if (channel.get_mut().send(Ok(data)).await).is_err() {
                        channel.remove();
                    }
                }
                Ok(None)
            }
            ChannelMessage::Close { token } => {
                tracing::debug!(target: "mux", ?token, "Channel closed by peer");
                if self.channels.remove(&token).is_none() {
                    tracing::warn!(target: "mux", ?token, "Channel already closed by peer");
                }
                Ok(None)
            }
        }
    }

    pub async fn open<R, W>(
        self: &Arc<Self>,
        open: OpenChannel,
    ) -> Result<(Token, Recver<R>, Sender<W>), Error>
    where
        R: Deserialize<'static> + 'static,
        W: Serialize,
    {
        let token = self.next_token();
        let mut message_sender = self.message_sender.clone();
        let (sender, recver) = mpsc::channel(8);

        let entry = self.channels.entry(token);
        if let Entry::Occupied(..) = &entry {
            return Err(Error::ChannelAlreadyOpen(token));
        }
        entry.insert(sender);

        let open = ChannelMessage::Open { token, open };
        if message_sender.send(open).await.is_err() {
            // unknown reason
            let error = io::ErrorKind::BrokenPipe.into();
            return Err(Error::SendOpen(token, error));
        };

        let recver = Recver {
            token,
            mux: self.clone(),
            stream: codec::FramedRead::new(
                StreamReader::new(recver),
                cbor_codec::CborDecoder::default(),
            ),
        };
        let sender = Sender {
            token,
            mux: self.clone(),
            sink: message_sender,
            _item: PhantomData,
        };
        Ok((token, recver, sender))
    }
}

impl Drop for Mux {
    fn drop(&mut self) {
        self.channels.clear();
    }
}

#[derive(Debug)]
pub struct NewChannel {
    token: Token,
    mux: Arc<Mux>,
    recver: mpsc::Receiver<io::Result<Bytes>>,
    open: OpenChannel,
}

impl NewChannel {
    pub fn request(&self) -> &OpenChannel {
        &self.open
    }

    pub fn token(&self) -> Token {
        self.token
    }

    pub fn assume<R, W>(self) -> (Recver<R>, Sender<W>)
    where
        R: Deserialize<'static>,
        W: Serialize,
    {
        let recver = Recver {
            token: self.token,
            mux: self.mux.clone(),
            stream: codec::FramedRead::new(
                StreamReader::new(self.recver),
                cbor_codec::CborDecoder::default(),
            ),
        };
        let sender = Sender {
            token: self.token,
            mux: self.mux.clone(),
            sink: self.mux.message_sender.clone(),
            _item: PhantomData,
        };
        (recver, sender)
    }
}

pin_project_lite::pin_project! {
    pub struct Recver<T: 'static> {
        token: Token,
        mux: Arc<Mux>,
        #[pin]
        stream: codec::FramedRead<
            StreamReader<mpsc::Receiver<io::Result<Bytes>>, Bytes>,
            cbor_codec::CborDecoder<'static, T>,
        >,
    }
}

impl<T: Deserialize<'static>> Stream for Recver<T> {
    type Item = io::Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project()
            .stream
            .poll_next(cx)
            .map_err(|_dee| io::ErrorKind::BrokenPipe.into())
    }
}

pin_project_lite::pin_project! {
    pub struct Sender<T> {
        token: Token,
        mux: Arc<Mux>,
        #[pin]
        sink: mpsc::Sender<ChannelMessage>,
        _item: PhantomData<T>,
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            token: self.token,
            mux: self.mux.clone(),
            sink: self.sink.clone(),
            _item: self._item,
        }
    }
}

impl<T: Serialize> Sink<T> for Sender<T> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project()
            .sink
            .poll_ready(cx)
            .map_err(|_se| io::ErrorKind::BrokenPipe.into())
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let project = self.project();
        project
            .sink
            .start_send(ChannelMessage::Data {
                token: *project.token,
                data: serde_cbor::to_vec(&item)
                    .map_err(|see| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("Failed to serialize item: {see:?}"),
                        )
                    })?
                    .into(),
            })
            .map_err(|_se| io::ErrorKind::BrokenPipe.into())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let project = self.project();
        project
            .sink
            .poll_flush(cx)
            .map_err(|_se| io::ErrorKind::BrokenPipe.into())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut project = self.project();
        ready!(
            (project.sink.as_mut().poll_ready(cx)).map_err(|se| io::Error::other(format!(
                "Mux sender failed to ready for Close: {se:?}"
            )))?
        );
        Poll::Ready(
            project
                .sink
                .start_send(ChannelMessage::Close {
                    token: *project.token,
                })
                .map_err(|_se| io::ErrorKind::BrokenPipe.into()),
        )
    }
}
