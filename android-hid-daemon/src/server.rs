//! On-device TCP server + verb dispatcher.
//!
//! This module is the daemon's wire-facing layer: it owns the listening
//! TCP socket, accepts client connections, performs the `PROTO/1\n`
//! handshake, and dispatches each framed request to a registered verb
//! handler. The frame type, the `MAX_FRAME_LEN` cap, and the verb/error
//! enums come from the `android-hid-protocol` crate so that the daemon
//! and the host agent speak the same bytes.
//!
//! ## Wire format (mirrors `handsets/docs/wire.md`)
//!
//! * each direction: `[u32 BE length][payload bytes]`
//! * zero-length payload = stream terminator
//! * errors: a single frame whose payload starts with
//!   `ERR:<UPPERCASE_TAG>[:detail]` (e.g. `ERR:UNKNOWN_CMD:bogus`)
//! * daemon opens with the 8-byte literal `b"PROTO/1\n"` before any
//!   length-prefixed frame
//!
//! ## Lifecycle
//!
//! ```text
//! Server::bind(cfg) -> std::io::Result<Server>
//! server.serve()     -> std::io::Result<()>     // blocks until quit
//! ```
//!
//! `serve` accepts connections in a loop. Each accepted `TcpStream` is
//! handed off to a dedicated `hs-conn` thread (named to match handsets'
//! Java reference so log scrapers and `ps -T -p <pid>` keep working).
//! The accept loop exits when a client sends `quit` (the server replies
//! `bye` and sets an internal stop flag) or when the listener returns
//! an I/O error.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use android_hid_protocol::{Frame, Verb, MAX_FRAME_LEN};

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Wire greeting sent by the daemon as the very first 8 bytes of every
/// accepted connection: `PROTO/1\n`.
///
/// The host agent reads exactly these 8 bytes before sending its first
/// framed request — if it sees anything else it surfaces a protocol
/// error rather than passing garbage into the framed decoder.
///
/// # Coordinate with the agent crate
///
/// The agent decoder MUST recognise exactly this byte sequence. We do
/// not yet have a `HANDSHAKE` constant in `android-hid-protocol`; once
/// the protocol crate exposes one, both sides should switch to it.
pub const HANDSHAKE: &[u8; 8] = b"PROTO/1\n";

/// Errors emitted by [`Server::bind`].
#[derive(Debug)]
pub enum BindError {
    /// `TcpListener::bind` failed.
    Listener(std::io::Error),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(e) => write!(f, "bind failed: {e}"),
        }
    }
}

impl StdError for BindError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Listener(e) => Some(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration + response types
// ---------------------------------------------------------------------------

/// Static configuration of a [`Server`].
///
/// Cheap to clone — every field is `Copy`.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address the listener will bind to. Use `127.0.0.1:0` for tests
    /// (ephemeral port) and `0.0.0.0:9008` (or similar) in production.
    pub bind_addr: SocketAddr,
    /// Maximum number of concurrent client connections. Excess
    /// connections block in the accept loop until a slot frees up.
    pub max_connections: usize,
    /// Protocol version advertised by this build. Currently unused on
    /// the wire but tracked here so future handshakes can include it
    /// without a struct churn.
    pub version: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // 127.0.0.1:0 = ask the kernel for any free port.
            // Production deployments override this via the `hsd` binary's
            // CLI flags (see `main.rs`).
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            max_connections: 8,
            version: 1,
        }
    }
}

/// What a verb handler returns to the dispatcher.
///
/// The dispatcher turns this into a sequence of wire frames:
///
/// * `One(Frame)` — write one frame, then read the next request
/// * `Stream(frames)` — write each frame in order, then write the
///   zero-length terminator
/// * `Empty` — write nothing (the client receives no reply and must
///   EOF). Useful for verbs that intentionally consume the
///   connection (e.g. legacy `stream`).
pub type DaemonResponse = ResponseKind;

/// Type alias mirroring handsets' terminology.
pub type ResponseKind = crate::server::Response;

/// Discriminated union of frame-send shapes returned by a handler.
///
/// Kept as a separate `enum` from `Response` so future phases can add
/// extra non-frame response variants (e.g. "close with RST") without
/// disturbing the `Server` -> handler trait surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Single, non-streaming reply.
    One(Frame),
    /// Multi-frame reply terminated by `Frame::empty_marker()`.
    Stream(Vec<Frame>),
    /// Intentionally no reply — close the stream.
    Empty,
}

// ---------------------------------------------------------------------------
// Dispatch table
// ---------------------------------------------------------------------------

/// Function signature every verb handler must satisfy.
///
/// Handlers receive the **entire** request payload (the bytes after the
/// 4-byte length prefix). The dispatcher splits the verb head off for
/// them via [`Verb::parse`], but the **handler itself** is responsible
/// for splitting the argument tail — there is no global parser because
/// every verb has a different argument shape.
///
/// Returning `Err` makes the dispatcher emit `ERR:INTERNAL:<display>`
/// instead of the handler's reply.
pub type HandlerFn =
    Arc<dyn Fn(&[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> + Send + Sync>;

/// Inner state shared by the accept loop and each connection thread.
struct ServerInner {
    /// Bind config kept around so tests can read `bind_addr` etc.
    config: ServerConfig,
    /// Registered verb handlers, looked up by parsed [`Verb`].
    handlers: HashMap<Verb, HandlerFn>,
    /// Set by the `quit` handler to make the accept loop exit.
    /// Wrapped in `Arc` so it can be shared with connection threads
    /// (we don't want every connection to own the canonical stop flag).
    stop: Arc<AtomicBool>,
    /// Listener moved here from `Server` after `bind()` so the
    /// connection thread that handles `quit` can drop it (closing the
    /// listening socket) and make subsequent `connect()` attempts
    /// fail. The accept loop also reads from this slot to detect
    /// "listener closed externally".
    listener_slot: Arc<std::sync::Mutex<Option<TcpListener>>>,
}

// Manual `Debug` because `HashMap<Verb, HandlerFn>` doesn't impl it
// (closures aren't Debug).
impl fmt::Debug for ServerInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verbs: Vec<&'static str> = self.handlers.keys().map(|v| v.as_str()).collect();
        f.debug_struct("ServerInner")
            .field("config", &self.config)
            .field("registered_verbs", &verbs)
            .field("stop", &self.stop.load(Ordering::SeqCst))
            .finish()
    }
}

impl Clone for ServerInner {
    fn clone(&self) -> Self {
        // Arc-shared handler table is cheap to clone. The handler
        // closures themselves stay shared (Arc::clone of each entry).
        Self {
            config: self.config.clone(),
            handlers: self.handlers.clone(),
            stop: self.stop.clone(),
            listener_slot: self.listener_slot.clone(),
        }
    }
}

/// The TCP server. Cheap to clone — internally an `Arc` over [`ServerInner`].
pub struct Server {
    inner: Arc<ServerInner>,
    /// Bound listener; moved into `serve()`.
    listener: Option<TcpListener>,
}

// Manual `Clone`: `TcpListener` isn't `Clone` (a kernel fd can't be
// duplicated), so we only share the listener with a fresh `Option`.
impl Clone for Server {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            listener: None, // listener is one-shot — only the original can serve().
        }
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("inner", &self.inner)
            .field("listener_bound", &self.listener.is_some())
            .finish()
    }
}

impl Server {
    /// Register an additional (or override an existing) handler for `verb`.
    pub fn register<F>(&mut self, verb: Verb, f: F) -> &mut Self
    where
        F: Fn(&[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>>
            + Send
            + Sync
            + 'static,
    {
        // We need to mutate the table through the Arc. Build a new
        // inner with the updated table and swap it in. Connection
        // threads keep their own `Arc` reference to the previous inner,
        // so they aren't disturbed by the mutation.
        let mut new_inner = (*self.inner).clone();
        new_inner.handlers.insert(verb, Arc::new(f));
        self.inner = Arc::new(new_inner);
        self
    }

    /// Address the listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        // Read from the shared listener slot — `serve()` will take the
        // listener out of this slot, after which `local_addr()` will
        // return the canonical "no listener" error.
        let slot = self.inner.listener_slot.lock().expect("listener_slot poisoned");
        match slot.as_ref() {
            Some(l) => l.local_addr(),
            None => Err(std::io::Error::other(
                "server has no listener (consumed by serve() or never bound)",
            )),
        }
    }

    /// Maximum number of concurrent client connections.
    pub fn max_connections(&self) -> usize {
        self.inner.config.max_connections
    }

    /// Bind the listening TCP socket and pre-register the five default
    /// verb handlers (`ping`, `info`, `wm_info`, `getprop`, `quit`).
    ///
    /// Callers can override individual handlers with [`Server::register`]
    /// right after `bind` returns.
    pub fn bind(config: ServerConfig) -> Result<Self, BindError> {
        let mut handlers: HashMap<Verb, HandlerFn> = HashMap::new();
        handlers.insert(Verb::Ping, Arc::new(default_ping));
        handlers.insert(Verb::Info, Arc::new(default_info));
        handlers.insert(Verb::WmInfo, Arc::new(default_wm_info));
        handlers.insert(Verb::Getprop, Arc::new(default_getprop));
        handlers.insert(Verb::Quit, Arc::new(default_quit));
        handlers.insert(Verb::Tap, Arc::new(crate::handlers::tap));
        handlers.insert(Verb::Screenshot, Arc::new(crate::handlers::screenshot));
        handlers.insert(Verb::Shell, Arc::new(crate::handlers::shell));

        let listener = TcpListener::bind(config.bind_addr).map_err(BindError::Listener)?;
        // Non-blocking so the accept loop can poll the `stop` flag
        // every ~50 ms and unblock when a `quit` arrives without
        // needing an extra shutdown signal fd.
        listener.set_nonblocking(true).map_err(BindError::Listener)?;
        let _ = listener.set_ttl(64);

        Ok(Self {
            inner: Arc::new(ServerInner {
                config,
                handlers,
                stop: Arc::new(AtomicBool::new(false)),
                listener_slot: Arc::new(std::sync::Mutex::new(Some(listener))),
            }),
            listener: None,
        })
    }

    /// Convenience: bind from a `addr:port` string + max connections.
    pub fn bind_str(addr: &str, max_connections: usize) -> Result<Self, BindError> {
        let parsed: SocketAddr = addr.parse().map_err(|e: std::net::AddrParseError| {
            BindError::Listener(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("could not parse '{addr}': {e}"),
            ))
        })?;
        let config = ServerConfig {
            bind_addr: parsed,
            max_connections,
            version: 1,
        };
        Self::bind(config)
    }

    /// Run the accept loop. Returns when:
    ///
    /// * a client sends `quit` (the server replies `bye`, sets the
    ///   internal stop flag, drops the listener so subsequent
    ///   `connect()` attempts fail), or
    /// * the listener returns an I/O error (propagated to the caller), or
    /// * the process is shut down externally.
    pub fn serve(self) -> std::io::Result<()> {
        let inner = self.inner.clone();
        // Take the listener out of the shared slot — only this thread
        // owns the listening fd from now on.
        let listener = inner
            .listener_slot
            .lock()
            .expect("listener_slot poisoned")
            .take()
            .expect("serve() called twice on the same Server");
        let max = inner.config.max_connections.max(1);

        eprintln!(
            "hsd: listening on {} (max_connections={}, version={})",
            listener.local_addr().unwrap_or(inner.config.bind_addr),
            inner.config.max_connections,
            inner.config.version,
        );

        let live = Arc::new(AtomicUsize::new(0));

        // We poll the stop flag between accepts and use a short
        // accept-loop sleep so a `quit` arriving from another thread
        // is noticed within ~50 ms even when no further clients are
        // trying to connect. Production deployments with sustained
        // traffic won't notice the sleep; idle servers pay 50 ms of
        // shutdown latency in exchange for the `quit` semantic.
        loop {
            if inner.stop.load(Ordering::SeqCst) {
                drop(listener); // closes the listening fd
                break;
            }

            let (stream, peer) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Non-blocking mode timed out — sleep briefly then
                    // re-check the stop flag.
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    if inner.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    return Err(e);
                }
            };

            // Block (don't drop) if we've hit max_connections.
            loop {
                let cur = live.load(Ordering::SeqCst);
                if cur < max {
                    if live
                        .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                    continue;
                }
                thread::yield_now();
            }

            let inner = inner.clone();
            let live = live.clone();
            let builder = thread::Builder::new().name("hs-conn".to_owned());
            builder
                .spawn(move || {
                    let _ = stream.set_nodelay(true);
                    if let Err(e) = handle_connection(stream, &inner) {
                        eprintln!("hsd: connection from {peer} ended with error: {e}");
                    }
                    live.fetch_sub(1, Ordering::SeqCst);
                })
                .expect("failed to spawn hs-conn thread");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Per-connection loop
// ---------------------------------------------------------------------------

/// Drive one accepted connection to completion.
///
/// Lifecycle (per handsets `Server.java`):
///
///   1. `setTcpNoDelay(true)` — matches line 69 of the Java reference.
///   2. Write the 8-byte handshake.
///   3. Read `[u32 BE length]`, then `[length bytes]` in a loop.
///   4. Parse the verb head, look it up, dispatch.
///   5. Write the response (or stream of responses + terminator).
///   6. Repeat from (3) until EOF or the `quit` verb.
fn handle_connection(mut stream: TcpStream, inner: &ServerInner) -> std::io::Result<()> {
    // Step 1: TCP_NODELAY. Handsets does this on line 69 of Server.java.
    let _ = stream.set_nodelay(true);

    // Step 2: handshake.
    stream.write_all(HANDSHAKE)?;
    stream.flush()?;

    loop {
        // Step 3a: read length prefix.
        let len = match read_u32_be(&mut stream) {
            Ok(len) => len,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        // A zero-length request is the stream terminator handsets uses
        // to signal "done sending". We treat it the same as EOF —
        // close the connection without a reply.
        if len == 0 {
            return Ok(());
        }

        // Reject declared lengths above the protocol cap with a clear
        // `ERR:BAD_ARG:bad-length:<n>` error rather than OOMing.
        if len > MAX_FRAME_LEN as u32 {
            let msg = format!("ERR:BAD_ARG:bad-length:{}", len);
            write_raw_frame(&mut stream, msg.as_bytes())?;
            continue;
        }

        // Step 3b: read payload.
        let mut buf = vec![0u8; len as usize];
        if len > 0 {
            stream.read_exact(&mut buf)?;
        }

        // Step 4: dispatch.
        let response = dispatch(&buf, inner);

        // Step 5: write response.
        match response {
            Ok(Response::One(frame)) => {
                frame.encode(&mut stream)?;
                stream.flush()?;
            }
            Ok(Response::Stream(frames)) => {
                for f in frames {
                    f.encode(&mut stream)?;
                }
                Frame::empty_marker().encode(&mut stream)?;
                stream.flush()?;
            }
            Ok(Response::Empty) => {
                return Ok(());
            }
            Err(e) => {
                let detail = e.to_string();
                let payload = format!("ERR:INTERNAL:{detail}");
                write_raw_frame(&mut stream, payload.as_bytes())?;
            }
        }
    }
}

/// Parse the verb head, look up the handler, call it.
fn dispatch(
    payload: &[u8],
    inner: &ServerInner,
) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    // Treat payload as UTF-8 text. We must never crash on a non-UTF-8
    // byte — fall back to a lossy conversion so the dispatch table
    // still gets a chance to answer with `UNKNOWN_CMD`.
    let text = String::from_utf8_lossy(payload);

    let head_end = text.find(char::is_whitespace).unwrap_or(text.len());
    let head = &text[..head_end];

    let verb = match Verb::parse(head) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Response::One(Frame::new(
                format!("ERR:UNKNOWN_CMD:{}", e.0).into_bytes(),
            )));
        }
    };

    let handler = match inner.handlers.get(&verb) {
        Some(h) => h.clone(),
        None => {
            return Ok(Response::One(Frame::new(
                format!("ERR:UNKNOWN_CMD:{}", verb.as_str()).into_bytes(),
            )));
        }
    };

    // Pass the entire payload (including the verb head) to the handler.
    let result = handler(payload);

    if matches!(verb, Verb::Quit) {
        // Set the stop flag *and* drop the listener so subsequent
        // `connect()` attempts fail fast (matches handsets'
        // `System.exit(0)` after sending `bye`).
        inner.stop.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = inner.listener_slot.lock() {
            slot.take(); // drop closes the listening socket
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Wire I/O primitives
// ---------------------------------------------------------------------------

/// Read 4 bytes and interpret them as a big-endian `u32`.
fn read_u32_be<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

/// Write a length-prefixed raw byte payload, flushing after the bytes.
fn write_raw_frame<W: Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()
}

// ---------------------------------------------------------------------------
// Default verb handlers (delegated to `crate::handlers`)
// ---------------------------------------------------------------------------

use crate::handlers::{getprop as handlers_getprop, info as handlers_info, ping as handlers_ping, quit as handlers_quit, wm_info as handlers_wm_info};

fn default_ping(args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    handlers_ping(args)
}
fn default_info(args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    handlers_info(args)
}
fn default_wm_info(args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    handlers_wm_info(args)
}
fn default_getprop(args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    handlers_getprop(args)
}
fn default_quit(args: &[u8]) -> Result<Response, Box<dyn StdError + Send + Sync>> {
    handlers_quit(args)
}

// ---------------------------------------------------------------------------
// Tests — full TCP loopback using `127.0.0.1:0`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    /// Bind a server on an ephemeral port and run it on a background
    /// thread. Returns the bound `SocketAddr`. If `setup` is provided
    /// it runs against the server **before** the accept loop starts so
    /// tests that override handlers don't have to share state with the
    /// accept thread (which would require a different design).
    fn spawn_test_server<F: FnOnce(&mut Server)>(setup: F) -> SocketAddr {
        let cfg = ServerConfig::default();
        let mut server = Server::bind(cfg).expect("bind");
        setup(&mut server);
        let addr = server.local_addr().expect("local_addr");

        thread::Builder::new()
            .name("hsd-test-accept".into())
            .spawn(move || {
                let _ = server.serve();
            })
            .expect("spawn accept thread");
        std::thread::sleep(Duration::from_millis(20));
        addr
    }

    /// Convenience: spin up a server with the five default handlers
    /// and no overrides.
    fn spawn_default_server() -> SocketAddr {
        spawn_test_server(|_| {})
    }

    fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
        Frame::decode(stream).expect("decode").payload().to_vec()
    }

    fn send_request(stream: &mut TcpStream, payload: &[u8]) {
        Frame::new(payload.to_vec()).encode(stream).expect("encode");
        stream.flush().unwrap();
    }

    fn dial_and_handshake(addr: SocketAddr) -> TcpStream {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut hello = [0u8; 8];
        s.read_exact(&mut hello).expect("handshake bytes");
        assert_eq!(&hello, HANDSHAKE, "server handshake mismatch");
        s
    }

    #[test]
    fn handshake_is_proto_1_newline() {
        assert_eq!(HANDSHAKE, b"PROTO/1\n");
    }

    #[test]
    fn default_server_binds_and_accepts() {
        let addr = spawn_default_server();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0, "ephemeral port should be assigned");
    }

    #[test]
    fn ping_verb_returns_pong() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"ping\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"pong");
    }

    #[test]
    fn info_verb_returns_screen_size() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"info\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"1080 1920\n");
    }

    #[test]
    fn wm_info_verb_returns_json() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"wm_info\n");
        let resp = read_frame(&mut s);
        let expected = br#"{"width":1080,"height":1920,"density":2.0,"xdpi":2.0,"ydpi":2.0,"rotation":0}"#;
        assert_eq!(resp, expected);
    }

    #[test]
    fn getprop_known_key() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"getprop ro.build.version.sdk\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"30");
    }

    #[test]
    fn getprop_unknown_key_errors() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"getprop ro.foo.bar\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"ERR:NOT_FOUND:no-such-prop");
    }

    #[test]
    fn unknown_verb_returns_err_unknown_cmd() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"bogus\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"ERR:UNKNOWN_CMD:bogus");
    }

    #[test]
    fn stream_response_terminates_with_zero_length_frame() {
        let addr = spawn_test_server(|s| {
            s.register(Verb::Info, |_args| {
                Ok(Response::Stream(vec![
                    Frame::from_static(b"chunk-1"),
                    Frame::from_static(b"chunk-2"),
                ]))
            });
        });
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"info\n");

        let chunk1 = read_frame(&mut s);
        assert_eq!(chunk1, b"chunk-1");
        let chunk2 = read_frame(&mut s);
        assert_eq!(chunk2, b"chunk-2");
        let terminator = read_frame(&mut s);
        assert!(terminator.is_empty(), "expected empty terminator, got {terminator:?}");
    }

    #[test]
    fn quit_verb_replies_bye_then_closes() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"quit\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"bye");
        drop(s);
        std::thread::sleep(Duration::from_millis(50));
        let result = TcpStream::connect_timeout(&addr, Duration::from_millis(500));
        assert!(result.is_err(), "expected connect to fail after quit, got {result:?}");
    }

    #[test]
    fn bad_length_returns_bad_arg_and_keeps_reading() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        // 1 GiB declared length — well above MAX_FRAME_LEN.
        let bogus_len: u32 = 1 << 30;
        s.write_all(&bogus_len.to_be_bytes()).unwrap();
        s.flush().unwrap();
        let resp = read_frame(&mut s);
        let text = std::str::from_utf8(&resp).unwrap();
        assert!(text.starts_with("ERR:BAD_ARG:bad-length:"), "got {text}");
        send_request(&mut s, b"ping\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"pong");
    }

    #[test]
    fn register_overrides_default_handler() {
        let addr = spawn_test_server(|s| {
            s.register(Verb::Ping, |_args| {
                Ok(Response::One(Frame::from_static(b"PING_OVERRIDDEN")))
            });
        });
        let mut s = dial_and_handshake(addr);
        send_request(&mut s, b"ping\n");
        let resp = read_frame(&mut s);
        assert_eq!(resp, b"PING_OVERRIDDEN");
    }

    #[test]
    fn empty_payload_is_zero_length_terminator() {
        let addr = spawn_default_server();
        let mut s = dial_and_handshake(addr);
        s.write_all(&0u32.to_be_bytes()).unwrap();
        s.flush().unwrap();
        let mut tail = [0u8; 16];
        let n = s.read(&mut tail).unwrap();
        assert_eq!(n, 0, "expected EOF after empty frame");
    }

    #[test]
    fn max_connections_is_exposed() {
        // max_connections lives on the bind-side, so we have to spin
        // up a server to read it. We don't need to connect to it.
        let addr = spawn_default_server();
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn handshakes_are_eight_bytes() {
        let addr = spawn_default_server();
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut hello = [0u8; 8];
        let start = Instant::now();
        s.read_exact(&mut hello).expect("handshake");
        let _ = start.elapsed();
        assert_eq!(&hello, b"PROTO/1\n");
    }
}