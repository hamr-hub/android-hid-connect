//! Control message types and serialization.

pub mod message;

pub use message::{
    ControlMessage, ControlMsgType, UhidCreate, UhidDestroy, UhidInput,
    InjectKeycode, InjectText, InjectTouchEvent, InjectScrollEvent,
    BackOrScreenOn, GetClipboard, SetClipboard, SetDisplayPower,
    StartApp, CameraSetTorch, ResizeDisplay,
    CONTROL_MSG_MAX_SIZE, INJECT_TEXT_MAX_LENGTH,
};
