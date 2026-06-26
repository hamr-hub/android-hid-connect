//! Control message types and serialization.

pub mod message;

pub use message::{
    AiConfig, AiQuery, BackOrScreenOn, CameraSetTorch, ControlMessage, ControlMsgType,
    GetClipboard, InjectKeycode, InjectScrollEvent, InjectText, InjectTouchEvent, ResizeDisplay,
    SetClipboard, SetDisplayPower, StartApp, UhidCreate, UhidDestroy, UhidInput, AI_FLAG_FEATURES,
    AI_FLAG_KEYFRAMES, AI_FLAG_MOTION, AI_FLAG_OBJECTS, AI_FLAG_TEXT, CONTROL_MSG_MAX_SIZE,
    INJECT_TEXT_MAX_LENGTH,
};
