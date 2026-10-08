use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use weft_proto::control::{
    self, ClientKind, ClientMessage, Empty, ErrorCode, Hello, ServerKind, ServerMessage, Welcome,
};
use weft_proto::{Host, Link};
use weft_session::stream::{self, io};
use weft_session::{StaticKeypair, tls};

use crate::settings::Transport;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HELLO_ID: u32 = u32::MAX;
const PING_ID: u32 = u32::MAX - 1;

#[derive(Debug)]
pub enum ControlEvent {
    Connected {
        welcome: Welcome,
        server: SocketAddr,
    },
    Refused(ErrorCode),
    Message(ServerMessage),
    /// Round trip of a ping over the control channel.
    Latency(Duration),
    Closed(String),
}

pub struct Control {
    commands: mpsc::UnboundedSender<ClientMessage>,
    task: JoinHandle<()>,
}

impl Control {
    pub fn spawn(
        link: Link,
        transport: Transport,
        keypair: Arc<StaticKeypair>,
        hello: Hello,
        generation: u64,
        events: mpsc::UnboundedSender<(u64, ControlEvent)>,
    ) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let reason = match run(link, transport, keypair, hello, generation, &events, rx).await {
                Ok(()) => "closed".to_string(),
                Err(error) => error.to_string(),
            };
            let _ = events.send((generation, ControlEvent::Closed(reason)));
        });
        Self { commands, task }
    }

    pub fn send(&self, message: ClientMessage) -> bool {
        self.commands.send(message).is_ok()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Session(#[from] weft_session::Error),
    #[error(transparent)]
    Decode(#[from] control::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("cannot resolve the server address")]
    Resolve,
    #[error("timed out")]
    Timeout,
    #[error("unexpected reply")]
    Protocol,
}

type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

async fn run(
    link: Link,
    transport: Transport,
    keypair: Arc<StaticKeypair>,
    hello: Hello,
    generation: u64,
    events: &mpsc::UnboundedSender<(u64, ControlEvent)>,
    mut commands: mpsc::UnboundedReceiver<ClientMessage>,
) -> Result<(), Error> {
    let host = match &link.host {
        Host::Domain(name) => name.clone(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    };
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), link.port)).await?.collect();
    addrs.sort_by_key(|addr| addr.is_ipv6());
    let server = *addrs.first().ok_or(Error::Resolve)?;

    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(server)).await.map_err(|_| Error::Timeout)??;
    stream.set_nodelay(true)?;
    let (mut reader, mut writer): (Reader, Writer) = match transport {
        Transport::Tls => {
            let connect = tls::connector().connect(tls::server_name(&host), stream);
            let stream = timeout(CONNECT_TIMEOUT, connect).await.map_err(|_| Error::Timeout)??;
            let (reader, writer) = tokio::io::split(stream);
            (Box::new(reader), Box::new(writer))
        }
        Transport::Raw => {
            let (reader, writer) = stream.into_split();
            (Box::new(reader), Box::new(writer))
        }
    };

    let (mut sender, mut receiver, welcome) = timeout(CONNECT_TIMEOUT, async {
        let (handshake, frame) = stream::connect(&keypair, link.server_key, SystemTime::now())?;
        writer.write_all(&frame).await?;
        let packet = io::read_handshake(&mut reader, handshake.own_obfs()).await?;
        let (mut sender, mut receiver) = handshake.finish(&packet)?.split();
        let hello = ClientMessage { id: HELLO_ID, kind: Some(ClientKind::Hello(hello)) };
        writer.write_all(&sender.seal(&control::encode(&hello))?).await?;
        let reply: ServerMessage = control::decode(&io::read_message(&mut reader, &mut receiver).await?)?;
        Ok::<_, Error>((sender, receiver, reply))
    })
    .await
    .map_err(|_| Error::Timeout)??;

    match welcome.kind {
        Some(ServerKind::Welcome(welcome)) if welcome.discovery_token.len() == control::DISCOVERY_TOKEN_LEN => {
            let _ = events.send((generation, ControlEvent::Connected { welcome, server }));
        }
        Some(ServerKind::Failure(failure)) => {
            let _ = events.send((generation, ControlEvent::Refused(failure.code())));
            return Ok(());
        }
        _ => return Err(Error::Protocol),
    }

    let reader_events = events.clone();
    let ping_sent: Arc<Mutex<Option<Instant>>> = Arc::default();
    let reader_ping = ping_sent.clone();
    let mut reader_task = AbortOnDrop(tokio::spawn(async move {
        loop {
            let bytes = timeout(IDLE_TIMEOUT, io::read_message(&mut reader, &mut receiver))
                .await
                .map_err(|_| Error::Timeout)??;
            let message: ServerMessage = control::decode(&bytes)?;
            let event = match reader_ping.lock().unwrap().take_if(|_| message.reply_to == PING_ID) {
                Some(sent) => ControlEvent::Latency(sent.elapsed()),
                None => ControlEvent::Message(message),
            };
            if reader_events.send((generation, event)).is_err() {
                return Ok::<_, Error>(());
            }
        }
    }));

    let mut ping = tokio::time::interval(PING_INTERVAL);
    loop {
        let message = tokio::select! {
            result = &mut reader_task.0 => return result.unwrap_or(Ok(())),
            command = commands.recv() => match command {
                Some(command) => command,
                None => return Ok(()),
            },
            _ = ping.tick() => {
                *ping_sent.lock().unwrap() = Some(Instant::now());
                ClientMessage { id: PING_ID, kind: Some(ClientKind::Ping(Empty {})) }
            }
        };
        writer.write_all(&sender.seal(&control::encode(&message))?).await?;
    }
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
