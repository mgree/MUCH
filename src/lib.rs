#![allow(dead_code)]

use rand::RngCore;
use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::time::DelayQueue;

use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};

use futures::SinkExt;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::stream::{Stream, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use tracing::{error, info, span, trace, Level};

pub const VERSION: &'static str = env!("CARGO_PKG_VERSION");

mod world;

use world::command::*;
use world::message::*;
use world::person::*;
use world::room::*;
use world::state::*;

pub type GameState = Arc<Mutex<State>>;

pub fn init() -> GameState {
    Arc::new(Mutex::new(State::new()))
}

////////////////////////////////////////////////////////////////////////////////
// TCP STUFF
////////////////////////////////////////////////////////////////////////////////

/// Internal messages for managing a peer's `MessageQueue`
#[derive(Clone, Debug)]
enum PeerMessage {
    LineFromPeer(String),
    SendToPeer(Message),
}

struct TCPPeer {
    /// Line-oriented TCP socket (poor-man's telnet)
    ///     
    /// This is the actual place we read from!
    // TODO support IAC codes, MCCP, etc.
    lines: Framed<TcpStream, LinesCodec>,
    /// Who this peer resolves to
    id: PersonId,
    /// Their name (cached, for convenience)
    name: String,
    /// Their locaation (cached, for convenience)
    loc: RoomId,
    /// Receive-end of the message queue for this connection
    rx: MessageQueueRX,
}

impl TCPPeer {
    async fn new(
        state: GameState,
        lines: Framed<TcpStream, LinesCodec>,
        person: &Person,
    ) -> io::Result<Self> {
        let addr = lines.get_ref().peer_addr()?;

        let (tx, rx) = mpsc::unbounded_channel();

        state
            .lock()
            .await
            .register_tcp_connection(person.id, addr, tx);

        Ok(TCPPeer {
            lines,
            id: person.id,
            name: person.name.clone(),
            loc: person.loc,
            rx,
        })
    }
}

impl Stream for TCPPeer {
    type Item = Result<PeerMessage, LinesCodecError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // send pending messages to the peer
        if let Poll::Ready(Some(v)) = Pin::new(&mut self.rx).poll_next(cx) {
            return Poll::Ready(Some(Ok(PeerMessage::SendToPeer(v))));
        }

        // connection-dependent read from the peer
        let result: Option<_> = futures::ready!(Pin::new(&mut self.lines).poll_next(cx));

        Poll::Ready(match result {
            Some(Ok(message)) => Some(Ok(PeerMessage::LineFromPeer(message))),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        })
    }
}

#[derive(Debug)]
struct LoginAbortedError {
    addr: SocketAddr,
    name: Option<String>,
}

impl Error for LoginAbortedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

impl fmt::Display for LoginAbortedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            None => write!(f, "Login error: connection with {} reset.", self.addr),
            Some(name) => write!(
                f,
                "Login error: connection with {} from {} reset.",
                name, self.addr
            ),
        }
    }
}

#[derive(Debug)]
struct TooManyPasswordAttemptsError {
    addr: SocketAddr,
    name: String,
}

impl Error for TooManyPasswordAttemptsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

impl fmt::Display for TooManyPasswordAttemptsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Login error: too many password attempts as {} from {}; connection reset.",
            self.name, self.addr
        )
    }
}

#[derive(Debug)]
struct PasswordsDontMatchError {
    addr: SocketAddr,
    name: String,
}

impl Error for PasswordsDontMatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

impl fmt::Display for PasswordsDontMatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Registration error: passwords don't match for {} on {}.",
            self.name, self.addr
        )
    }
}

pub async fn prompt<F, Ferr, Ftimeout>(
    lines: &mut Framed<TcpStream, LinesCodec>,
    prompt: &str,
    reprompt: &str,
    valid: F,
    check_tries: Ferr,
    timeout: Ftimeout,
) -> Result<String, Box<dyn Error>>
where
    F: Fn(&str) -> bool,
    Ferr: Fn(usize) -> Option<Box<dyn Error>>,
    Ftimeout: FnOnce() -> Box<dyn Error>,
{
    let mut num_tries = 0;
    loop {
        lines.send(prompt).await?;

        match lines.next().await {
            Some(Ok(line)) => {
                let line = line.trim();

                if valid(&line) {
                    return Ok(line.to_string());
                }

                num_tries += 1;
                if let Some(error) = check_tries(num_tries) {
                    return Err(error);
                }

                lines.send(reprompt).await?;
            }
            _ => return Err(timeout()),
        }
    }
}

pub async fn login(
    state: GameState,
    lines: &mut Framed<TcpStream, LinesCodec>,
    addr: SocketAddr,
) -> Result<Person, Box<dyn Error>> {
    // TODO welcome header, instructions, etc.

    let name = prompt(
        lines,
        "What is your email address or Twitter handle? ",
        "Please enter a valid email address or Twitter handle.",
        |name| !name.is_empty() && name.contains('@'),
        |_| None, // unlimited tries
        || Box::new(LoginAbortedError { addr, name: None }),
    )
    .await?;

    let conn = Connection::TCP { addr };
    let p = state.lock().await.person_by_name(&name);

    match p {
        Some(person) => {
            let _password = prompt(
                lines,
                "Password: ",
                "Password incorrect.",
                |password| {
                    argon2::verify_encoded(&person.password, password.as_bytes()).unwrap_or(false)
                },
                |failed_tries| {
                    if failed_tries >= 3 {
                        Some(Box::new(TooManyPasswordAttemptsError {
                            name: name.clone(),
                            addr,
                        }))
                    } else {
                        None
                    }
                },
                || {
                    Box::new(LoginAbortedError {
                        addr,
                        name: Some(name.clone()),
                    })
                },
            )
            .await?;

            return Ok(Person::new(&person, conn));
        }
        None => loop {
            let password1 = prompt(
                lines,
                "Please enter a password: ",
                "That is not a valid password. It should be at least 8 characters.",
                |password| password.len() >= 8,
                |_| None,
                || {
                    Box::new(LoginAbortedError {
                        addr,
                        name: Some(name.clone()),
                    })
                },
            )
            .await?;

            lines.send("Please re-enter your password: ").await?;

            match lines.next().await {
                Some(Ok(password2)) => {
                    if password1 != password2.trim() {
                        lines.send("Passwords don't match.").await?;
                        continue;
                    }

                    let person = state.lock().await.new_person(&name, &password1);
                    return Ok(Person::new(&person, conn));
                }
                _ => {
                    return Err(Box::new(LoginAbortedError {
                        addr,
                        name: Some(name),
                    }))
                }
            }
        },
    };
}

pub async fn process(
    state: GameState,
    stream: TcpStream,
    addr: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let mut lines = Framed::new(stream, LinesCodec::new());

    let person = login(state.clone(), &mut lines, addr).await?;
    let mut peer = TCPPeer::new(state.clone(), lines, &person).await?;

    let span = span!(Level::INFO, "session");
    let _guard = span.enter();
    info!(peer.id, "login");

    state.lock().await.arrive(&person, peer.loc).await;

    while let Some(result) = peer.next().await {
        match result {
            Ok(PeerMessage::LineFromPeer(msg)) => {
                let cmd = Command::parse(msg)?;

                cmd.run(state.clone(), peer.loc, peer.id, &peer.name).await;
            }

            Ok(PeerMessage::SendToPeer(msg)) => {
                if let Some(loc) = msg.new_location(peer.id) {
                    peer.loc = loc;
                }
                let s = msg.render(peer.id).await;
                peer.lines.send(s).await?;
            }

            Err(e) => {
                error!(?e, id = peer.id);
            }
        }
    }

    {
        let mut state = state.lock().await;

        // actually log them off
        state.unregister_tcp_connection(peer.id, addr);

        // announce it to everyone
        state.depart(&person, peer.loc).await;
    }
    info!(id = peer.id, "logout");


    trace!("disconnected");
    Ok(())
}

pub async fn tcp_serve<A: ToSocketAddrs>(state: Arc<Mutex<State>>, addr: A) -> io::Result<()> {
    let mut listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, addr) = listener.accept().await?;

        let span = span!(Level::INFO, "TCP connection");
        let _guard = span.enter();
        info!(?addr, "connected");

        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = process(state, stream, addr).await {
                error!(?e);
            }
        });
    }
}

////////////////////////////////////////////////////////////////////////////////
// HTTP STUFF
////////////////////////////////////////////////////////////////////////////////

/// The cookie in which we store sessions
const SESSIONID: &'static str = "id";

/// The name of the CSRF token variable for POST requests
const CSRFTOKEN: &'static str = "tok";

/// Time-to-live in a room between calls to `/api/be`
const HTTP_TTL_SECS: u64 = 30;

pub type SessionId = String;

type CSRFToken = String;

pub struct HTTPState {
    /// CSPRNG for session and CSRF tokens
    csprng: rand::rngs::StdRng,
    sessions: HashMap<SessionId, PersonId>,
    tokens: HashMap<SessionId, CSRFToken>,
    // TODO call reset on a hit to /do or /be
    // TODO someone needs to be polling this queue and dropping people from rooms
    timeouts: DelayQueue<(SessionId, RoomId)>,
}

impl HTTPState {
    pub fn new() -> Self {
        HTTPState {
            csprng: rand::SeedableRng::from_rng(rand::thread_rng()).unwrap(),
            sessions: HashMap::new(),
            tokens: HashMap::new(),
            timeouts: DelayQueue::new(),
        }
    }

    fn gen_token(&mut self) -> String {
        // generate random value
        let mut buf: [u8; 16] = [0; 16];
        self.csprng.fill_bytes(&mut buf);

        // make it text
        base64::encode(buf)
    }

    pub fn gen_session_id_for(&mut self, id: PersonId) -> CSRFToken {
        let session = self.gen_token();

        // record the session
        self.sessions.insert(session.clone(), id);

        session
    }

    pub fn gen_csrf_token_for(&mut self, session: SessionId) -> SessionId {
        let token = self.gen_token();

        // record the token for the session
        // TODO if we already have one... old pages are now out of date... keep a set of them?
        self.tokens.insert(session.clone(), token.clone());

        token
    }
}

pub async fn http_serve<A: std::net::ToSocketAddrs + std::fmt::Display>(
    state: Arc<Mutex<State>>,
    addr_spec: A,
) -> Result<(), Box<dyn Error + Send>> {
    let mut addrs = addr_spec.to_socket_addrs().unwrap();
    let addr = addrs.next().unwrap();
    assert_eq!(
        addrs.next(),
        None,
        "expected a unique bind location for the HTTP server, but {} resolves to at least two",
        addr_spec
    );

    let make_svc = make_service_fn(move |conn: &AddrStream| {
        let state = state.clone();
        let remote_addr = conn.remote_addr();

        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                http_route(state.clone(), remote_addr, req)
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);
    match server.await {
        Ok(()) => Ok(()),
        Err(e) => Err(Box::new(e)),
    }
}

async fn http_route(
    state: Arc<Mutex<State>>,
    client: SocketAddr,
    req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    let span = span!(Level::INFO, "HTTP request", client = ?client, method = ?req.method(), uri = ?req.uri());
    let _guard = span.enter();

    let mut resp = Response::new(Body::empty());

    // TODO session info
    // need to thread a session table through everywhere (keep it separate from the state? it's HTTP only...)
    // see if cookie exists. if not, generate a new session (and store it in the table)
    // if so, get peer information appropriately (in the handler? not everyone needs the info...)

    trace!("routing");
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => http_unimplemented(state, req, &mut resp).await,

        (&Method::GET, "/register") => http_unimplemented(state, req, &mut resp).await,
        (&Method::POST, "/register") => http_unimplemented(state, req, &mut resp).await,

        (&Method::GET, "/user") => http_unimplemented(state, req, &mut resp).await,
        (&Method::GET, "/room") => http_unimplemented(state, req, &mut resp).await,

        (&Method::GET, "/who") => http_unimplemented(state, req, &mut resp).await,
        (&Method::GET, "/help") => http_unimplemented(state, req, &mut resp).await,

        (&Method::GET, "/admin") => http_unimplemented(state, req, &mut resp).await,

        // TODO cache-control on these end points
        (&Method::GET, "/api/be") => http_unimplemented(state, req, &mut resp).await,
        (&Method::POST, "/api/do") => http_unimplemented(state, req, &mut resp).await,
        (&Method::POST, "/api/leave") => http_unimplemented(state, req, &mut resp).await,
        (&Method::POST, "/api/login") => http_unimplemented(state, req, &mut resp).await,
        (&Method::POST, "/api/logout") => http_unimplemented(state, req, &mut resp).await,
        (&Method::POST, "/api/who") => http_unimplemented(state, req, &mut resp).await,
        _ => {
            *resp.status_mut() = StatusCode::NOT_FOUND;
            *resp.body_mut() = Body::from("404 Not Found");
        }
    };

    info!(status = ?resp.status());
    Ok(resp)
}

async fn http_unimplemented(
    _state: Arc<Mutex<State>>,
    _req: Request<Body>,
    resp: &mut Response<Body>,
) {
    *resp.status_mut() = StatusCode::NOT_IMPLEMENTED;
    *resp.body_mut() = Body::from("501 Not Implemented");
}
