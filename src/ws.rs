// A minimal WebSocket client (RFC 6455), enough for the DevTools Protocol: text messages,
// fragmented frames and large payloads. No TLS, no extensions.
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::net::TcpStream;

pub struct Reader(TcpStream);
pub struct Writer(TcpStream);

fn random_u64() -> u64 {
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

/// Connects to ws://host:port/path.
pub fn connect(url: &str) -> io::Result<(Reader, Writer)> {
    let rest = url.strip_prefix("ws://").ok_or_else(|| io::Error::other(format!("not a ws:// url: {url}")))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut s = TcpStream::connect(host)?;
    s.set_nodelay(true)?;
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&random_u64().to_le_bytes());
    key[8..].copy_from_slice(&random_u64().to_le_bytes());
    let req = format!(
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        crate::base64_encode(&key)
    );
    s.write_all(req.as_bytes())?;
    // Read the response headers one byte at a time so no frame bytes are consumed.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b)? == 0 || head.len() > 16384 {
            return Err(io::Error::other("websocket handshake failed"));
        }
        head.push(b[0]);
    }
    let status = String::from_utf8_lossy(&head);
    if !status.starts_with("HTTP/1.1 101") {
        return Err(io::Error::other(format!("websocket handshake refused: {}", status.lines().next().unwrap_or(""))));
    }
    Ok((Reader(s.try_clone()?), Writer(s)))
}

impl Writer {
    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        let payload = text.as_bytes();
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x81); // FIN + text
        let n = payload.len();
        if n < 126 {
            frame.push(0x80 | n as u8);
        } else if n <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mask = (random_u64() as u32).to_le_bytes();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.0.write_all(&frame)
    }
}

impl Reader {
    /// The next complete text or binary message. Fails when the connection closes.
    pub fn recv(&mut self) -> io::Result<String> {
        let mut message = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.0.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let opcode = h[0] & 0x0f;
            let masked = h[1] & 0x80 != 0;
            let mut len = (h[1] & 0x7f) as u64;
            if len == 126 {
                let mut e = [0u8; 2];
                self.0.read_exact(&mut e)?;
                len = u16::from_be_bytes(e) as u64;
            } else if len == 127 {
                let mut e = [0u8; 8];
                self.0.read_exact(&mut e)?;
                len = u64::from_be_bytes(e);
            }
            let mut mask = [0u8; 4];
            if masked {
                self.0.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.0.read_exact(&mut payload)?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Err(io::ErrorKind::ConnectionAborted.into()),
                0x9 | 0xA => continue, // ping, pong: DevTools does not need answers
                _ => message.extend_from_slice(&payload),
            }
            if fin {
                return Ok(String::from_utf8_lossy(&message).into_owned());
            }
        }
    }
}
