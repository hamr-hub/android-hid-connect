//! `HidClient` — parallel command submission via `std::sync::mpsc`.
//!
//! The motivation: `HidSession` is `Send` but all its input methods
//! take `&mut self`, so multiple producer threads would otherwise
//! need a `Mutex` — which serializes the 1ms coalescing window we
//! built in `coalesce::CoalescingWriter`. `HidClient` solves this by
//! handing the `HidSession` to a single dispatcher thread and letting
//! other threads push commands over a bounded channel.
//!
//! Pattern:
//!
//! ```no_run
//! use android_hid_connect::session::{HidSession, OpenRequest};
//! use android_hid_connect::client::HidCommand;
//! use android_hid_connect::transport::open_tcp;
//!
//! let sock = open_tcp("127.0.0.1", 27183).unwrap();
//! let s = HidSession::open(sock, OpenRequest::all()).unwrap();
//! let (client, dispatcher) = s.into_client().unwrap();
//!
//! let c = client.clone();
//! std::thread::spawn(move || {
//!     c.send(HidCommand::TypeText("hello".into())).unwrap();
//! });
//!
//! client.send(HidCommand::MultitouchDown { id: 0, x: 540, y: 1200, pressure: 1.0 }).unwrap();
//! client.send(HidCommand::MultitouchUp { id: 0 }).unwrap();
//!
//! client.close();
//! let _sock = dispatcher.join().unwrap();
//! ```

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use crate::control::message::{
    ControlMessage, GetClipboard, SetClipboard, SetDisplayPower, StartApp,
};
use crate::error::{Error, Result, TransportWrite};
use crate::session::HidSession;
use crate::types::{GamepadAxis, GamepadButton, Modifiers};

/// Default channel bound for `HidClient`. Bounds the back-pressure
/// between producers and the dispatcher.
pub const DEFAULT_CHANNEL_BOUND: usize = 1024;

/// Every operation the AI can request. Cheap to construct; the
/// dispatcher routes each variant to the right `HidSession` method.
#[derive(Debug, Clone)]
pub enum HidCommand {
    TypeText(String),
    Key {
        scancode: u8,
        pressed: bool,
        mods: Modifiers,
    },
    MultitouchDown {
        id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    },
    MultitouchMove {
        id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    },
    MultitouchUp {
        id: u64,
    },
    GamepadButton {
        btn: GamepadButton,
        pressed: bool,
    },
    GamepadStick {
        axis: GamepadAxis,
        value: f32,
    },
    SetScreenPower {
        on: bool,
    },
    LaunchApp {
        name: String,
    },
    SetClipboard {
        text: String,
        paste: bool,
    },
    /// Phase 1 stub: sends the GET_CLIPBOARD request and replies
    /// with an empty string. True server-reply forwarding is a
    /// follow-up run.
    GetClipboard {
        reply: std::sync::mpsc::Sender<String>,
        copy_key: u8,
    },
    Flush,
    Close,
}

/// Producer side of the parallel control channel. `Clone` = additional
/// producer to the same channel. `Send` but not `Sync` (mpsc isn't);
/// use `Arc<HidClient>` if needed.
#[derive(Debug, Clone)]
pub struct HidClient {
    tx: SyncSender<HidCommand>,
}

/// Handle for joining the dispatcher thread and recovering the
/// underlying transport.
pub struct HidDispatcher<T: TransportWrite + Send + 'static> {
    join: Option<JoinHandle<Result<T>>>,
}

impl<T: TransportWrite + Send + 'static> std::fmt::Debug for HidDispatcher<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidDispatcher").finish_non_exhaustive()
    }
}

impl<T: TransportWrite + Send + 'static> HidDispatcher<T> {
    pub fn join(mut self) -> Result<T> {
        let j = self
            .join
            .take()
            .ok_or(Error::DispatcherDown("already joined"))?;
        match j.join() {
            Ok(r) => r,
            Err(_) => Err(Error::DispatcherDown("thread panicked")),
        }
    }
}

impl<T: TransportWrite + Send + 'static> HidSession<T> {
    /// Move this session into a background dispatcher thread and
    /// return a `HidClient` + `HidDispatcher`. The session's
    /// `CoalescingWriter` keeps batching inside the dispatcher.
    pub fn into_client(self) -> Result<(HidClient, HidDispatcher<T>)> {
        self.into_client_with_bound(DEFAULT_CHANNEL_BOUND)
    }

    pub fn into_client_with_bound(self, bound: usize) -> Result<(HidClient, HidDispatcher<T>)> {
        let (tx, rx) = mpsc::sync_channel::<HidCommand>(bound);
        let join = thread::Builder::new()
            .name("android-hid-dispatcher".into())
            .spawn(move || dispatcher_loop(self, rx))
            .map_err(|e| Error::Transport(format!("dispatcher spawn: {e}")))?;
        Ok((HidClient { tx }, HidDispatcher { join: Some(join) }))
    }
}

impl HidClient {
    pub fn try_send(&self, cmd: HidCommand) -> Result<()> {
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::SessionLifecycle("channel full (back-pressure)"),
            mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
        })
    }

    pub fn send(&self, cmd: HidCommand) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| Error::DispatcherDown("channel disconnected"))
    }

    pub fn close(&self) {
        let _ = self.tx.try_send(HidCommand::Close);
    }
}

fn dispatcher_loop<T: TransportWrite + Send>(
    mut session: HidSession<T>,
    rx: Receiver<HidCommand>,
) -> Result<T> {
    loop {
        let cmd = match rx.recv() {
            Ok(c) => c,
            Err(_) => {
                let _ = session.close();
                return Ok(session.into_inner());
            }
        };
        match cmd {
            HidCommand::Close => {
                let _ = session.close();
                return Ok(session.into_inner());
            }
            HidCommand::TypeText(s) => {
                let _ = session.type_text(&s);
            }
            HidCommand::Key {
                scancode,
                pressed,
                mods,
            } => {
                let _ = session.key(scancode, pressed, mods);
            }
            HidCommand::MultitouchDown { id, x, y, pressure } => {
                let _ = session.multitouch().down(id, x, y, pressure);
            }
            HidCommand::MultitouchMove { id, x, y, pressure } => {
                let _ = session.multitouch().move_to(id, x, y, pressure);
            }
            HidCommand::MultitouchUp { id } => {
                let _ = session.multitouch().up(id);
            }
            HidCommand::GamepadButton { btn, pressed } => {
                let _ = session.set_button(btn, pressed);
            }
            HidCommand::GamepadStick { axis, value } => {
                let _ = session.set_stick(axis, value);
            }
            HidCommand::SetScreenPower { on } => {
                let _ = session.send(&ControlMessage::SetDisplayPower(SetDisplayPower { on }));
            }
            HidCommand::LaunchApp { name } => {
                let _ = session.send(&ControlMessage::StartApp(StartApp { name }));
            }
            HidCommand::SetClipboard { text, paste } => {
                let _ = session.send(&ControlMessage::SetClipboard(SetClipboard {
                    sequence: 0,
                    paste,
                    text,
                }));
            }
            HidCommand::GetClipboard { reply, copy_key } => {
                let _ = session.send(&ControlMessage::GetClipboard(GetClipboard { copy_key }));
                let _ = reply.send(String::new());
            }
            HidCommand::Flush => {
                let _ = session.flush_now();
            }
        }
    }
}
