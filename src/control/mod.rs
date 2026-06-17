//! Control message types and serialization.

pub mod message;

pub use message::{
    BackOrScreenOn, CameraSetTorch, ControlMessage, ControlMsgType, GetClipboard, InjectKeycode,
    InjectScrollEvent, InjectText, InjectTouchEvent, ResizeDisplay, SetClipboard, SetDisplayPower,
    StartApp, UhidCreate, UhidDestroy, UhidInput, CONTROL_MSG_MAX_SIZE, INJECT_TEXT_MAX_LENGTH,
};
