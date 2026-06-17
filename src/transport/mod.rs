//! Transport layer — anything that can be written to (TCP socket,
//! in-memory buffer, mock for tests).
//!
//! The transport is decoupled from the [`crate::control::ControlMessage`]
//! serialization so callers can use the same builder API against real
//! network sockets or test doubles.

use std::io::Write;

use crate::control::message::ControlMessage;
use crate::error::Result;
use crate::error::TransportWrite;

/// Send a single control message over the transport.
pub fn send_one<W: TransportWrite>(transport: &mut W, msg: &ControlMessage) -> Result<()> {
    let bytes = msg.serialize()?;
    transport.write_all(&bytes)?;
    transport.flush()
}

/// Send a batch of control messages in order, returning the first error
/// if any. Non-droppable messages (UHID_CREATE / UHID_DESTROY) are not
/// retried — the caller is expected to retry the whole batch on
/// failure.
pub fn send_batch<W: TransportWrite>(transport: &mut W, msgs: &[ControlMessage]) -> Result<()> {
    for m in msgs {
        send_one(transport, m)?;
    }
    Ok(())
}

/// Convenience: open a TCP connection to the given `host:port` (typical
/// use: `127.0.0.1:27183` after `adb forward tcp:27183 localabstract:scrcpy`).
pub fn open_tcp(host: &str, port: u16) -> std::io::Result<std::net::TcpStream> {
    use std::net::ToSocketAddrs;
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address"))?;
    let stream = std::net::TcpStream::connect(addr)?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

/// In-memory transport for tests. Wraps a `Vec<u8>` and exposes the
/// collected bytes.
#[derive(Debug, Default, Clone)]
pub struct MockTransport {
    pub bytes: Vec<u8>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for MockTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::message::{UhidCreate, UhidDestroy, UhidInput};
    use crate::types::HID_MAX_SIZE;

    #[test]
    fn mock_collects_bytes() {
        let mut t = MockTransport::new();
        send_one(&mut t, &ControlMessage::UhidDestroy(UhidDestroy { id: 1 })).unwrap();
        assert_eq!(t.bytes, vec![14, 0x00, 0x01]);
    }

    #[test]
    fn batch_is_sequential() {
        let mut t = MockTransport::new();
        let mut data = [0u8; HID_MAX_SIZE];
        data[0] = 0x02;
        let msgs = vec![
            ControlMessage::UhidInput(UhidInput {
                id: 1,
                size: 8,
                data,
            }),
            ControlMessage::UhidDestroy(UhidDestroy { id: 1 }),
        ];
        send_batch(&mut t, &msgs).unwrap();
        // First message: type(1) + id(2) + size(2) + data(8) = 13
        // Second message: type(1) + id(2) = 3
        assert_eq!(t.bytes.len(), 13 + 3);
    }

    #[test]
    fn open_tcp_works_for_localhost_unbound_port() {
        // Bind a listener and grab the port, then connect to it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _accept = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let stream = open_tcp("127.0.0.1", port).expect("connect");
        // No specific assertion on the stream — just that it connected.
        drop(stream);
    }

    #[test]
    fn create_message_serialization() {
        let mut t = MockTransport::new();
        let msg = ControlMessage::UhidCreate(UhidCreate {
            id: 1,
            vendor_id: 0,
            product_id: 0,
            name: Some("Test".to_string()),
            report_desc: vec![0x05, 0x01],
        });
        send_one(&mut t, &msg).unwrap();
        // type(1) + id(2) + vid(2) + pid(2) + name_len(1) + name(4) + rd_len(2) + rd(2) = 16
        assert_eq!(t.bytes.len(), 16);
    }
}
