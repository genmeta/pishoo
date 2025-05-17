#![allow(unused)]

use std::{
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
use futures::{Sink, SinkExt, Stream, channel::mpsc};
use serde::{Deserialize, Serialize};
use tokio::io;
use tokio_util::{
    codec::{self},
    io::StreamReader,
};

use super::{Error, cbor_codec};

#[derive(Debug, Display, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Token(u64);

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Client,
    Server,
}

impl Token {
    pub fn new(seq: u64, role: Role) -> Self {
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
    token: AtomicU64,
    channels: DashMap<Token, mpsc::Sender<io::Result<Bytes>>>,
    message_sender: mpsc::Sender<ChannelMessage>,
}

impl Mux {
    pub fn new(message_sender: mpsc::Sender<ChannelMessage>, role: Role) -> Self {
        Mux {
            channels: DashMap::new(),
            message_sender,
            token: AtomicU64::new(Token::new(0, role).0),
        }
    }

    fn token(&self) -> Token {
        Token(self.token.load(SeqCst))
    }

    fn next_token(&self) -> Token {
        let token = self.token.fetch_add(2, SeqCst);
        Token(token)
    }

    pub async fn receive(
        self: &Arc<Self>,
        message: ChannelMessage,
    ) -> Result<Option<NewChannel>, Error> {
        tracing::trace!(target: "sshd", "Received message: {message:?}");
        match message {
            ChannelMessage::Open { token, open } => {
                if token.role() == self.token().role() {
                    return Err(
                        format!("Failed to resolve message Open {open:?}: wrong role").into(),
                    );
                }
                let (sender, recver) = mpsc::channel(8);
                let entry = self.channels.entry(token);
                if let Entry::Occupied(..) = &entry {
                    return Err(format!("Failed to open channel: {token}: already open").into());
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
                tracing::debug!(target: "sshd", ?token, "Closed by peer");
                if self.channels.remove(&token).is_none() {
                    return Err(format!("Failed to close channel: {token}: already closed").into());
                }
                Ok(None)
            }
        }
    }

    pub async fn open<R: Deserialize<'static> + 'static, W: Serialize>(
        self: &Arc<Self>,
        open: OpenChannel,
    ) -> Result<(Token, Recver<'static, R>, Sender<W>), Error> {
        let token = self.next_token();
        let mut message_sender = self.message_sender.clone();
        let (sender, recver) = mpsc::channel(8);

        let entry = self.channels.entry(token);
        if let Entry::Occupied(..) = &entry {
            return Err(format!("Failed to open channel: {token}: already open").into());
        }
        entry.insert(sender);
        if let Err(e) = message_sender
            .send(ChannelMessage::Open { token, open })
            .await
        {
            return Err(
                format!("Failed to open channel: {token}: Failed to send Open: {e:?}").into(),
            );
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

    pub fn assume<'de, R: Deserialize<'de>, W: Serialize>(self) -> (Recver<'de, R>, Sender<W>) {
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
    pub struct Recver<'de, T> {
        token: Token,
        mux: Arc<Mux>,
        #[pin]
        stream: codec::FramedRead<
            StreamReader<mpsc::Receiver<io::Result<Bytes>>, Bytes>,
            cbor_codec::CborDecoder<'de, T>,
        >,
    }
}

impl<'de, T: Deserialize<'de>> Stream for Recver<'de, T> {
    type Item = io::Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project()
            .stream
            .poll_next(cx)
            .map_err(|dee| io::Error::new(io::ErrorKind::InvalidData, dee))
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
            .map_err(|se| io::Error::other(format!("Mux sender failed to ready: {se:?}")))
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
            .map_err(|se| io::Error::other(format!("Failed to send: {se:?}")))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let project = self.project();
        project
            .sink
            .poll_flush(cx)
            .map_err(|se| io::Error::other(format!("Mux sedner failed to flush : {se:?}")))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut project = self.project();
        tracing::debug!(target: "sshd", token=?project.token, "Closed by local");
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
                .map_err(|se| io::Error::other(format!("Mux sedner failed to send Close: {se:?}"))),
        )
    }
}
