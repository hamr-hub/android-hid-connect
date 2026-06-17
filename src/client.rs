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
use crate::coalesce::DIRECT_GAMEPAD_BATCH_FRAMES;
use crate::error::{Error, Result, TransportWrite};
use crate::session::{GamepadFrameRaw, GAMEPAD_FRAME_BYTES, HidSession};
use crate::types::{GamepadAxis, GamepadButton, Modifiers};

/// Default channel bound for `HidClient`. Bounds the back-pressure
/// between producers and the dispatcher. A larger default reduces
/// back-pressure spikes on high-rate gamepad loops while keeping
/// memory usage bounded in normal use.
pub const DEFAULT_CHANNEL_BOUND: usize = 4096;

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
    /// Replace the full gamepad button frame in one call. Bit layout
    /// follows `GamepadButton` (including dpad flags).
    GamepadButtons {
        buttons: u32,
    },
    GamepadStick {
        axis: GamepadAxis,
        value: f32,
    },
    GamepadStickRaw {
        axis: GamepadAxis,
        value: i16,
    },
    GamepadLeftStickRaw {
        x: i16,
        y: i16,
    },
    GamepadRightStickRaw {
        x: i16,
        y: i16,
    },
    GamepadTriggersRaw {
        left: i16,
        right: i16,
    },
    GamepadSticksRaw {
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    },
    /// Replace the full gamepad frame (buttons + both sticks + both triggers).
    GamepadFrameRaw {
        buttons: u32,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    },
    /// Replace a single full gamepad frame without server-side dedupe.
    GamepadFrameRawUnchecked(GamepadFrameRaw),
    /// Replace multiple full frames in one command.
    GamepadFrameRawBatch(Vec<GamepadFrameRaw>),
    /// Replace multiple full gamepad frames in a fixed stack buffer.
    GamepadFrameRawBatchFixed {
        len: u8,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    },
    /// Replace multiple full gamepad frames without server-side dedupe,
    /// using a fixed stack buffer.
    GamepadFrameRawBatchFixedUnchecked {
        len: u8,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    },
    /// Replace multiple full frames without server-side dedupe.
    GamepadFrameRawBatchUnchecked(Vec<GamepadFrameRaw>),
    /// Replace a full frame in one fast-path command with a pre-packed
    /// 15-byte gamepad report.
    GamepadPackedFrame([u8; GAMEPAD_FRAME_BYTES]),
    /// Replace multiple full frames in one fast-path command with
    /// pre-packed 15-byte gamepad reports.
    GamepadPackedFrameBatch(Vec<[u8; GAMEPAD_FRAME_BYTES]>),
    /// Replace multiple packed frames in one fast-path command with
    /// a fixed stack buffer.
    GamepadPackedFrameBatchFixed {
        len: u8,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
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

    /// Send one gamepad button edge.
    pub fn send_button(&self, btn: GamepadButton, pressed: bool) -> Result<()> {
        self.send(HidCommand::GamepadButton { btn, pressed })
    }

    /// Replace all gamepad button bits in one command.
    pub fn send_buttons(&self, buttons: u32) -> Result<()> {
        self.send(HidCommand::GamepadButtons { buttons })
    }

    /// Send one normalized gamepad axis sample.
    ///
    /// Forwarded to dispatcher as `GamepadStick`, which keeps a single
    /// conversion step in one hot path.
    pub fn send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.send(HidCommand::GamepadStick { axis, value })
    }

    /// Send one raw gamepad axis sample.
    pub fn send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.send(HidCommand::GamepadStickRaw { axis, value })
    }

    /// Send both left-stick axes in one command.
    pub fn send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.send(HidCommand::GamepadLeftStickRaw { x, y })
    }

    /// Send both right-stick axes in one command.
    pub fn send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.send(HidCommand::GamepadRightStickRaw { x, y })
    }

    /// Send both trigger axes in one command.
    pub fn send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.send(HidCommand::GamepadTriggersRaw { left, right })
    }

    /// Send both sticks + both triggers in one command.
    pub fn send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.send(HidCommand::GamepadSticksRaw {
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        })
    }

    pub fn send(&self, cmd: HidCommand) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| Error::DispatcherDown("channel disconnected"))
    }

    /// Send a batch of full gamepad frames through one channel send.
    /// Useful when your loop already produced many consecutive
    /// frame samples and you want to reduce dispatch overhead.
    pub fn send_frame_batch(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.send_frame(frames[0]);
        }
        self.send(HidCommand::GamepadFrameRawBatch(frames))
    }

    /// Send one full gamepad frame with server-side dedupe.
    ///
    /// Use this when your loop wants unchanged-frame suppression even
    /// when going through `HidClient`.
    pub fn send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.send(HidCommand::GamepadFrameRaw {
            buttons: frame.buttons,
            left_x: frame.left_x,
            left_y: frame.left_y,
            right_x: frame.right_x,
            right_y: frame.right_y,
            left_trigger: frame.left_trigger,
            right_trigger: frame.right_trigger,
        })
    }

    fn send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
        dedupe: bool,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        if len == 1 {
            let frame = frames[0];
            if dedupe {
                return self.send_frame(frame);
            }
            return self.send_frame_unchecked(frame);
        }
        if dedupe {
            self.send(HidCommand::GamepadFrameRawBatchFixed {
                len: len as u8,
                frames,
            })
        } else {
            self.send(HidCommand::GamepadFrameRawBatchFixedUnchecked {
                len: len as u8,
                frames,
            })
        }
    }

    fn try_send_frame_batch_fixed_internal(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
        dedupe: bool,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        if len == 1 {
            let frame = frames[0];
            if dedupe {
                return self.try_send_frame(frame);
            }
            return self.try_send_frame_unchecked(frame);
        }
        let cmd = if dedupe {
            HidCommand::GamepadFrameRawBatchFixed {
                len: len as u8,
                frames,
            }
        } else {
            HidCommand::GamepadFrameRawBatchFixedUnchecked {
                len: len as u8,
                frames,
            }
        };
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => {
                Error::SessionLifecycle("channel full (back-pressure)")
            }
            mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
        })?;
        Ok(())
    }

    /// Send a batch of full gamepad frames without state-dedupe.
    ///
    /// Use this when the caller owns a complete frame stream and wants
    /// every frame to be written, even if duplicates occur.
    pub fn send_frame_batch_unchecked(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.send_frame_unchecked(frames[0]);
        }
        self.send(HidCommand::GamepadFrameRawBatchUnchecked(frames))
    }

    /// Send one full gamepad frame without state-dedupe.
    ///
    /// Use this when your loop already owns the whole frame and wants
    /// every sample on the wire.
    pub fn send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.send(HidCommand::GamepadFrameRawUnchecked(frame))
    }

    /// Non-blocking single-frame unchecked send.
    ///
    /// Drops to `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadFrameRawUnchecked(frame))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking gamepad button edge.
    pub fn try_send_button(&self, btn: GamepadButton, pressed: bool) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadButton { btn, pressed })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking button-bitframe update.
    pub fn try_send_buttons(&self, buttons: u32) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadButtons { buttons })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking normalized axis update.
    pub fn try_send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadStick { axis, value })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking raw axis update.
    pub fn try_send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadStickRaw { axis, value })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking left-stick update.
    pub fn try_send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadLeftStickRaw { x, y })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking right-stick update.
    pub fn try_send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadRightStickRaw { x, y })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking trigger-pair update.
    pub fn try_send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadTriggersRaw { left, right })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking full-axis + trigger update.
    pub fn try_send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadSticksRaw {
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking single-frame send with server-side dedupe.
    ///
    /// Drops to `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadFrameRaw {
                buttons: frame.buttons,
                left_x: frame.left_x,
                left_y: frame.left_y,
                right_x: frame.right_x,
                right_y: frame.right_y,
                left_trigger: frame.left_trigger,
                right_trigger: frame.right_trigger,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Send one packed 15-byte gamepad report through one channel send.
    pub fn send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.send(HidCommand::GamepadPackedFrame(frame))
    }

    /// Send packed 15-byte gamepad frames through one channel send.
    /// This is the lowest-overhead path when the upstream loop already
    /// emits raw HID payloads.
    pub fn send_frame_packed_batch(
        &self,
        frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    ) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.send_frame_packed(frames[0]);
        }
        self.send(HidCommand::GamepadPackedFrameBatch(frames))
    }

    fn send_frame_packed_batch_fixed_internal(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame packed batch fixed length overflow"));
        }
        if len == 1 {
            return self.send_frame_packed(frames[0]);
        }
        self.send(HidCommand::GamepadPackedFrameBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Send packed 15-byte gamepad frames with a fixed stack buffer.
    ///
    /// Use when your loop already keeps a `[[u8; 15]; 32]` ring and
    /// wants to avoid `Vec` allocation in the hot path.
    pub fn send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.send_frame_packed_batch_fixed_internal(len, frames)
    }

    /// Non-blocking packed batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_packed_batch(
        &self,
        frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    ) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.try_send_frame_packed(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadPackedFrameBatch(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking packed fixed-buffer batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame packed batch fixed length overflow"));
        }
        if len == 1 {
            return self.try_send_frame_packed(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadPackedFrameBatchFixed {
                len: len as u8,
                frames,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking packed frame send.
    pub fn try_send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadPackedFrame(frame))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking batch send. Drops to `SessionLifecycle` when the
    /// internal queue is full.
    pub fn try_send_frame_batch(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.try_send_frame(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadFrameRawBatch(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking full-frame batch send without state-dedupe.
    ///
    /// Use this when frame cadence matters more than transport payload
    /// suppression and drops due to queue-full can be handled upstream.
    pub fn try_send_frame_batch_unchecked(
        &self,
        frames: Vec<GamepadFrameRaw>,
    ) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.try_send_frame_unchecked(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadFrameRawBatchUnchecked(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }

    /// Non-blocking fixed-size deduped frame batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.try_send_frame_batch_fixed_internal(len, frames, true)
    }

    /// Non-blocking fixed-size unchecked frame batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.try_send_frame_batch_fixed_internal(len, frames, false)
    }

    pub fn close(&self) {
        let _ = self.tx.send(HidCommand::Close);
    }

    /// Flush any pending coalesced UHID_INPUT writes immediately.
    pub fn flush(&self) -> Result<()> {
        self.send(HidCommand::Flush)
    }

    /// Non-blocking flush request for coalesced UHID_INPUT writes.
    pub fn try_flush(&self) -> Result<()> {
        self.tx
            .try_send(HidCommand::Flush)
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
            })?;
        Ok(())
    }
}

/// Batched sender for high-rate gamepad frame loops sharing one
/// `HidClient` channel.
///
/// Repeated `push` calls stay on the caller thread and only cross the
/// channel when the local batch is full or you explicitly `flush`es.
/// For loops already emitting one frame each tick, this can drop
/// dispatcher lock contention substantially.
#[derive(Debug)]
pub struct GamepadFrameBatcher<'a> {
    client: &'a HidClient,
    fixed_frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    fixed_len: usize,
    frames: Vec<GamepadFrameRaw>,
    dedupe: bool,
    batch_size: usize,
    use_fixed: bool,
}

impl<'a> GamepadFrameBatcher<'a> {
    /// Create a batcher that sends full-state-dedupe `set_frame_raw_batch`
    /// calls when it flushes.
    pub fn dedupe(client: &'a HidClient, batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        let use_fixed = batch_size <= DIRECT_GAMEPAD_BATCH_FRAMES;
        Self {
            client,
            fixed_frames: [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES],
            fixed_len: 0,
            frames: if use_fixed { Vec::new() } else { Vec::with_capacity(batch_size) },
            dedupe: true,
            batch_size,
            use_fixed,
        }
    }

    /// Create a batcher that sends un-deduped `set_frame_raw_batch_unchecked`
    /// calls when it flushes.
    pub fn unchecked(client: &'a HidClient, batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        let use_fixed = batch_size <= DIRECT_GAMEPAD_BATCH_FRAMES;
        Self {
            client,
            fixed_frames: [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES],
            fixed_len: 0,
            frames: if use_fixed { Vec::new() } else { Vec::with_capacity(batch_size) },
            dedupe: false,
            batch_size,
            use_fixed,
        }
    }

    /// Push one frame into the local batch.
    ///
    /// Returns only transport errors. When the batch is full this
    /// automatically flushes before returning.
    pub fn push(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                self.flush()?;
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Push one frame into the local batch using non-blocking channel
    /// semantics. On producer backlog, returns `SessionLifecycle`.
    pub fn try_push(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                if let Err(err) = self.try_flush() {
                    return Err(err);
                }
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.try_flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.try_flush()?;
        }
        Ok(())
    }

    /// Push several frames into the local batch in one step.
    pub fn push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = GamepadFrameRaw>,
    {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of frames with fewer boundary checks than
    /// per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[GamepadFrameRaw]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several frames into the local batch in one step using
    /// non-blocking channel semantics.
    pub fn try_push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = GamepadFrameRaw>,
    {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice with non-blocking flush semantics.
    pub fn try_push_many_slice(&mut self, frames: &[GamepadFrameRaw]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Send any queued frames now.
    pub fn flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = if self.dedupe {
                    self.client.send_frame(frame)
                } else {
                    self.client.send_frame_unchecked(frame)
                };
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch =
                [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = self.client.send_frame_batch_fixed(len, batch, self.dedupe);
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = if self.dedupe {
                self.client.send_frame(frame)
            } else {
                self.client.send_frame_unchecked(frame)
            };
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::replace(&mut self.frames, Vec::new());
        let result = if self.dedupe {
            self.client.send_frame_batch(frames)
        } else {
            self.client.send_frame_batch_unchecked(frames)
        };
        if let Err(err) = result {
            self.frames = frames;
            return Err(err);
        }
        self.frames = frames;
        self.frames.clear();
        Ok(())
    }

    /// Send any queued frames now using non-blocking channel semantics.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = if self.dedupe {
                    self.client.try_send_frame(frame)
                } else {
                    self.client.try_send_frame_unchecked(frame)
                };
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch =
                [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = if self.dedupe {
                self.client.try_send_frame_batch_fixed(len, batch)
            } else {
                self.client.try_send_frame_batch_fixed_unchecked(len, batch)
            };
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = if self.dedupe {
                self.client.try_send_frame(frame)
            } else {
                self.client.try_send_frame_unchecked(frame)
            };
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::replace(&mut self.frames, Vec::new());
        let result = if self.dedupe {
            self.client.try_send_frame_batch(frames)
        } else {
            self.client.try_send_frame_batch_unchecked(frames)
        };
        if let Err(err) = result {
            self.frames = frames;
            return Err(err);
        }
        self.frames = frames;
        self.frames.clear();
        Ok(())
    }

    /// Current number of buffered frames.
    pub fn len(&self) -> usize {
        if self.use_fixed {
            return self.fixed_len;
        }
        self.frames.len()
    }

    /// Returns `true` when there is no buffered frame.
    pub fn is_empty(&self) -> bool {
        if self.use_fixed {
            return self.fixed_len == 0;
        }
        self.frames.is_empty()
    }
}

impl<'a> Drop for GamepadFrameBatcher<'a> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Batched sender for high-rate packed 15-byte gamepad frame loops sharing
/// one `HidClient` channel.
///
/// This avoids per-batch `Vec` allocation by using a fixed stack buffer when
/// the batch size is at most `DIRECT_GAMEPAD_BATCH_FRAMES`.
#[derive(Debug)]
pub struct PackedGamepadFrameBatcher<'a> {
    client: &'a HidClient,
    fixed_frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    fixed_len: usize,
    frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    batch_size: usize,
    use_fixed: bool,
}

impl<'a> PackedGamepadFrameBatcher<'a> {
    /// Create a batcher for pre-packed 15-byte gamepad reports.
    pub fn new(client: &'a HidClient, batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        let use_fixed = batch_size <= DIRECT_GAMEPAD_BATCH_FRAMES;
        Self {
            client,
            fixed_frames: [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
            fixed_len: 0,
            frames: if use_fixed { Vec::new() } else { Vec::with_capacity(batch_size) },
            batch_size,
            use_fixed,
        }
    }

    /// Push one packed report into the local batch.
    ///
    /// Returns only transport errors. When the batch is full this
    /// automatically flushes before returning.
    pub fn push(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                self.flush()?;
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "packed frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Push one packed report into the local batch using non-blocking channel
    /// semantics. On producer backlog, returns `SessionLifecycle`.
    pub fn try_push(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                if let Err(err) = self.try_flush() {
                    return Err(err);
                }
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "packed frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.try_flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.try_flush()?;
        }
        Ok(())
    }

    /// Push several packed reports into the local batch in one step.
    pub fn push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = [u8; GAMEPAD_FRAME_BYTES]>,
    {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of packed reports with fewer boundary checks
    /// than per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[[u8; GAMEPAD_FRAME_BYTES]]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "packed frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several packed reports into the local batch in one step using
    /// non-blocking channel semantics.
    pub fn try_push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = [u8; GAMEPAD_FRAME_BYTES]>,
    {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of packed reports with non-blocking flush
    /// semantics.
    pub fn try_push_many_slice(&mut self, frames: &[[u8; GAMEPAD_FRAME_BYTES]]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "packed frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Send any queued frames now.
    pub fn flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = self.client.send_frame_packed(frame);
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch = [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = self.client.send_frame_packed_batch_fixed(len, batch);
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = self.client.send_frame_packed(frame);
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::replace(&mut self.frames, Vec::new());
        let result = self.client.send_frame_packed_batch(frames);
        if let Err(err) = result {
            self.frames = frames;
            return Err(err);
        }
        self.frames = frames;
        self.frames.clear();
        Ok(())
    }

    /// Send any queued frames now using non-blocking channel semantics.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = self.client.try_send_frame_packed(frame);
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch = [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = self.client.try_send_frame_packed_batch_fixed(len, batch);
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = self.client.try_send_frame_packed(frame);
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::replace(&mut self.frames, Vec::new());
        let result = self.client.try_send_frame_packed_batch(frames);
        if let Err(err) = result {
            self.frames = frames;
            return Err(err);
        }
        self.frames = frames;
        self.frames.clear();
        Ok(())
    }

    /// Current number of buffered frames.
    pub fn len(&self) -> usize {
        if self.use_fixed {
            self.fixed_len
        } else {
            self.frames.len()
        }
    }

    /// Returns `true` when there is no buffered frame.
    pub fn is_empty(&self) -> bool {
        if self.use_fixed {
            self.fixed_len == 0
        } else {
            self.frames.is_empty()
        }
    }
}

impl<'a> Drop for PackedGamepadFrameBatcher<'a> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn dispatcher_loop<T: TransportWrite + Send>(
    mut session: HidSession<T>,
    rx: Receiver<HidCommand>,
) -> Result<T> {
    loop {
        let first = match rx.recv() {
            Ok(c) => c,
            Err(_) => {
                let _ = session.close();
                return Ok(session.into_inner());
            }
        };
        if dispatch_to_session(first, &mut session) {
            return Ok(session.into_inner());
        }
        while let Ok(cmd) = rx.try_recv() {
            if dispatch_to_session(cmd, &mut session) {
                return Ok(session.into_inner());
            }
        }
    }
}

fn dispatch_to_session<T: TransportWrite + Send>(
    cmd: HidCommand,
    session: &mut HidSession<T>,
) -> bool {
    match cmd {
        HidCommand::Close => {
            let _ = session.close();
            true
        }
        HidCommand::TypeText(s) => {
            let _ = session.type_text(&s);
            false
        }
        HidCommand::Key {
            scancode,
            pressed,
            mods,
        } => {
            let _ = session.key(scancode, pressed, mods);
            false
        }
        HidCommand::MultitouchDown { id, x, y, pressure } => {
            let _ = session.multitouch().down(id, x, y, pressure);
            false
        }
        HidCommand::MultitouchMove { id, x, y, pressure } => {
            let _ = session.multitouch().move_to(id, x, y, pressure);
            false
        }
        HidCommand::MultitouchUp { id } => {
            let _ = session.multitouch().up(id);
            false
        }
        HidCommand::GamepadButton { btn, pressed } => {
            let _ = session.set_button(btn, pressed);
            false
        }
        HidCommand::GamepadButtons { buttons } => {
            let _ = session.set_buttons(buttons);
            false
        }
        HidCommand::GamepadStick { axis, value } => {
            let _ = session.set_stick(axis, value);
            false
        }
        HidCommand::GamepadStickRaw { axis, value } => {
            let _ = session.set_stick_raw(axis, value);
            false
        }
        HidCommand::GamepadLeftStickRaw { x, y } => {
            let _ = session.set_left_stick_raw(x, y);
            false
        }
        HidCommand::GamepadRightStickRaw { x, y } => {
            let _ = session.set_right_stick_raw(x, y);
            false
        }
        HidCommand::GamepadTriggersRaw { left, right } => {
            let _ = session.set_triggers_raw(left, right);
            false
        }
        HidCommand::GamepadSticksRaw {
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        } => {
            let _ = session.set_sticks_raw(
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            );
            false
        }
        HidCommand::GamepadFrameRaw {
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        } => {
            let _ = session.set_frame_raw(
                buttons,
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            );
            false
        }
        HidCommand::GamepadFrameRawUnchecked(frame) => {
            let _ = session.set_frame_raw_unchecked_frame(frame);
            false
        }
        HidCommand::GamepadFrameRawBatch(frames) => {
            let _ = session.set_frame_raw_batch(&frames);
            false
        }
        HidCommand::GamepadFrameRawBatchFixed { len, frames } => {
            let _ = session.set_frame_raw_batch(&frames[..len as usize]);
            false
        }
        HidCommand::GamepadFrameRawBatchFixedUnchecked { len, frames } => {
            let _ = session.set_frame_raw_batch_unchecked(&frames[..len as usize]);
            false
        }
        HidCommand::GamepadFrameRawBatchUnchecked(frames) => {
            let _ = session.set_frame_raw_batch_unchecked(&frames);
            false
        }
        HidCommand::GamepadPackedFrame(frame) => {
            let _ = session.set_frame_raw_packed(&frame);
            false
        }
        HidCommand::GamepadPackedFrameBatch(frames) => {
            let _ = session.set_frame_raw_packed_batch(&frames);
            false
        }
        HidCommand::GamepadPackedFrameBatchFixed { len, frames } => {
            let _ = session.set_frame_raw_packed_batch(&frames[..len as usize]);
            false
        }
        HidCommand::SetScreenPower { on } => {
            let _ = session.send(&ControlMessage::SetDisplayPower(SetDisplayPower { on }));
            false
        }
        HidCommand::LaunchApp { name } => {
            let _ = session.send(&ControlMessage::StartApp(StartApp { name }));
            false
        }
        HidCommand::SetClipboard { text, paste } => {
            let _ = session.send(&ControlMessage::SetClipboard(SetClipboard {
                sequence: 0,
                paste,
                text,
            }));
            false
        }
        HidCommand::GetClipboard { reply, copy_key } => {
            let _ = session.send(&ControlMessage::GetClipboard(GetClipboard { copy_key }));
            let _ = reply.send(String::new());
            false
        }
        HidCommand::Flush => {
            let _ = session.flush_now();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;
    use crate::error::Error;
    use crate::session::GAMEPAD_FRAME_BYTES;

    #[test]
    fn gamepad_frame_batcher_fixed_try_push_stays_safe_when_channel_full() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = GamepadFrameBatcher::dedupe(&client, 2);

        assert!(batcher.try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0)).is_ok());
        assert!(batcher.try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0)).is_ok());
        assert!(batcher.try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0)).is_ok());
        assert!(batcher.try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0)).is_err());

        let overflow_err = batcher
            .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
            .unwrap_err();
        assert!(matches!(overflow_err, Error::SessionLifecycle(_)));
    }

    #[test]
    fn packed_gamepad_frame_batcher_fixed_try_push_stays_safe_when_channel_full() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = PackedGamepadFrameBatcher::new(&client, 2);
        let frame = [1u8; GAMEPAD_FRAME_BYTES];

        assert!(batcher.try_push(frame).is_ok());
        assert!(batcher.try_push(frame).is_ok());
        assert!(batcher.try_push(frame).is_ok());
        assert!(batcher.try_push(frame).is_err());

        let overflow_err = batcher.try_push(frame).unwrap_err();
        assert!(matches!(overflow_err, Error::SessionLifecycle(_)));
    }

    #[test]
    fn send_frame_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = GamepadFrameRaw::new(0x0001, 10, 20, 30, 40, 50, 60);

        client.send_frame_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRaw {
                buttons,
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            } => {
                assert_eq!(buttons, frame.buttons);
                assert_eq!(left_x, frame.left_x);
                assert_eq!(left_y, frame.left_y);
                assert_eq!(right_x, frame.right_x);
                assert_eq!(right_y, frame.right_y);
                assert_eq!(left_trigger, frame.left_trigger);
                assert_eq!(right_trigger, frame.right_trigger);
            }
            other => panic!("expected GamepadFrameRaw command, got {other:?}"),
        }
    }

    #[test]
    fn send_frame_batch_unchecked_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = GamepadFrameRaw::new(0x0002, 11, 21, 31, 41, 51, 61);

        client.send_frame_batch_unchecked(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRawUnchecked(cmd_frame) => {
                assert_eq!(cmd_frame, frame);
            }
            other => panic!("expected GamepadFrameRawUnchecked command, got {other:?}"),
        }
    }

    #[test]
    fn send_frame_packed_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = [0xABu8; GAMEPAD_FRAME_BYTES];

        client.send_frame_packed_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadPackedFrame(cmd_frame) => assert_eq!(cmd_frame, frame),
            other => panic!("expected GamepadPackedFrame command, got {other:?}"),
        }
    }

    #[test]
    fn try_send_frame_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = GamepadFrameRaw::new(0x0003, 12, 22, 32, 42, 52, 62);

        client.try_send_frame_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRaw {
                buttons,
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            } => {
                assert_eq!(buttons, frame.buttons);
                assert_eq!(left_x, frame.left_x);
                assert_eq!(left_y, frame.left_y);
                assert_eq!(right_x, frame.right_x);
                assert_eq!(right_y, frame.right_y);
                assert_eq!(left_trigger, frame.left_trigger);
                assert_eq!(right_trigger, frame.right_trigger);
            }
            other => panic!("expected GamepadFrameRaw command, got {other:?}"),
        }
    }

    #[test]
    fn try_send_frame_packed_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = [0xCDu8; GAMEPAD_FRAME_BYTES];

        client.try_send_frame_packed_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadPackedFrame(cmd_frame) => assert_eq!(cmd_frame, frame),
            other => panic!("expected GamepadPackedFrame command, got {other:?}"),
        }
    }

    #[test]
    fn gamepad_frame_batcher_flush_single_frame_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = GamepadFrameBatcher::dedupe(&client, 4);
        let frame = GamepadFrameRaw::new(0x0001, 10, 20, 30, 40, 50, 60);

        batcher.push(frame).unwrap();
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRaw { buttons, left_x, left_y, right_x, right_y, left_trigger, right_trigger } => {
                assert_eq!(buttons, frame.buttons);
                assert_eq!(left_x, frame.left_x);
                assert_eq!(left_y, frame.left_y);
                assert_eq!(right_x, frame.right_x);
                assert_eq!(right_y, frame.right_y);
                assert_eq!(left_trigger, frame.left_trigger);
                assert_eq!(right_trigger, frame.right_trigger);
            }
            other => panic!("expected GamepadFrameRaw command, got {other:?}"),
        }
    }

    #[test]
    fn packed_gamepad_frame_batcher_flush_single_frame_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = PackedGamepadFrameBatcher::new(&client, 4);
        let frame = [1u8; GAMEPAD_FRAME_BYTES];

        batcher.push(frame).unwrap();
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::GamepadPackedFrame(cmd_frame) => assert_eq!(cmd_frame, frame),
            other => panic!("expected GamepadPackedFrame command, got {other:?}"),
        }
    }

    #[test]
    fn send_gamepad_input_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(4);
        let client = HidClient { tx };

        client.send_button(GamepadButton::South, true).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadButton {
                btn: GamepadButton::South,
                pressed: true,
            } => {}
            other => panic!("expected GamepadButton command, got {other:?}"),
        }

        client.send_buttons(GamepadButton::South as u32).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadButtons {
                buttons: GamepadButton::South as u32,
            } => {}
            other => panic!("expected GamepadButtons command, got {other:?}"),
        }

        client.send_stick_raw(GamepadAxis::LeftX, 123).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadStickRaw {
                axis: GamepadAxis::LeftX,
                value: 123,
            } => {}
            other => panic!("expected GamepadStickRaw command, got {other:?}"),
        }

        client
            .send_sticks_raw(1, 2, 3, 4, 5, 6)
            .unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadSticksRaw {
                left_x: 1,
                left_y: 2,
                right_x: 3,
                right_y: 4,
                left_trigger: 5,
                right_trigger: 6,
            } => {}
            other => panic!("expected GamepadSticksRaw command, got {other:?}"),
        }
    }

    #[test]
    fn try_send_gamepad_input_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(4);
        let client = HidClient { tx };

        client.try_send_left_stick_raw(7, -7).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadLeftStickRaw { x: 7, y: -7 } => {}
            other => panic!("expected GamepadLeftStickRaw command, got {other:?}"),
        }

        client.try_send_right_stick_raw(-8, 8).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadRightStickRaw { x: -8, y: 8 } => {}
            other => panic!("expected GamepadRightStickRaw command, got {other:?}"),
        }

        client.try_send_stick(GamepadAxis::RightY, 0.25).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadStick {
                axis: GamepadAxis::RightY,
                value,
            } => assert_eq!(value, 0.25),
            other => panic!("expected GamepadStick command, got {other:?}"),
        }
    }

    #[test]
    fn flush_command_round_trip() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };

        client.flush().unwrap();
        client.try_flush().unwrap();

        assert!(matches!(rx.try_recv().unwrap(), HidCommand::Flush));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::Flush));
    }
}
