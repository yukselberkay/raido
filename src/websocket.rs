//! The little of WebSocket (RFC 6455) a local server needs: the opening handshake,
//! messages in one frame or several, and pings answered. Enough for Claude Code's
//! IDE connection (see `ide`), without another dependency.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
};

pub const CLOSE: u8 = 0x8;
pub const PONG: u8 = 0xa;
const TEXT: u8 = 0x1;
/// Messages larger than this end the connection.
const MAX: usize = 32 << 20;

/// The handshake request's headers, names in lower case.
pub fn read_request(stream: &mut impl Read) -> io::Result<HashMap<String, String>> {
    let mut head = Vec::new();
    let mut byte = [0];
    // Byte by byte: nothing past the blank line is consumed.
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 << 10 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too long",
            ));
        }
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    if !lines.next().is_some_and(|line| line.starts_with("GET ")) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a GET"));
    }
    Ok(lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect())
}

/// The answer to a handshake with this `Sec-WebSocket-Key`, taking `protocol` when
/// the client asked for one.
pub fn accept(key: &str, protocol: Option<&str>) -> String {
    let mut digest = key.as_bytes().to_vec();
    digest.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let mut response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
        base64(&sha1(&digest))
    );
    if let Some(protocol) = protocol {
        response.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    response.push_str("\r\n");
    response
}

pub enum Message {
    Text(String),
    Binary,
    Close,
}

/// The next message; pings are answered through `pong` (the payload to send back).
pub fn read_message(stream: &mut impl Read, mut pong: impl FnMut(&[u8])) -> io::Result<Message> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let mut data = Vec::new();
    let mut kind = TEXT;
    loop {
        let mut head = [0; 2];
        stream.read_exact(&mut head)?;
        let (fin, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0f);
        let length = match head[1] & 0x7f {
            126 => {
                let mut bytes = [0; 2];
                stream.read_exact(&mut bytes)?;
                u64::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0; 8];
                stream.read_exact(&mut bytes)?;
                u64::from_be_bytes(bytes)
            }
            n => u64::from(n),
        };
        if length > MAX as u64 || data.len() + length as usize > MAX {
            return Err(invalid("message too large"));
        }
        let mut mask = [0; 4];
        if head[1] & 0x80 != 0 {
            stream.read_exact(&mut mask)?;
        }
        let mut payload = vec![0; length as usize];
        stream.read_exact(&mut payload)?;
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
        match opcode {
            0x0..=0x2 => {
                if opcode != 0 {
                    kind = opcode;
                }
                data.extend_from_slice(&payload);
                if fin {
                    return Ok(match kind {
                        TEXT => Message::Text(
                            String::from_utf8(data).map_err(|_| invalid("text not UTF-8"))?,
                        ),
                        _ => Message::Binary,
                    });
                }
            }
            CLOSE => return Ok(Message::Close),
            0x9 => pong(&payload),
            PONG => {}
            _ => return Err(invalid("unknown frame")),
        }
    }
}

/// Send one unmasked frame (servers don't mask).
pub fn write_frame(stream: &mut impl Write, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= 0xffff => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

pub fn write_text(stream: &mut impl Write, text: &str) -> io::Result<()> {
    write_frame(stream, TEXT, text.as_bytes())
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for chunk in message.chunks(64) {
        let mut w = [0u32; 80];
        for (word, bytes) in w.iter_mut().zip(chunk.chunks(4)) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0; 20];
    for (bytes, x) in out.chunks_mut(4).zip(h) {
        bytes.copy_from_slice(&x.to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
    const LETTERS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &byte)| n | u32::from(byte) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                LETTERS[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_answers_as_the_rfc_example_does() {
        // RFC 6455, section 1.3.
        let response = accept("dGhlIHNhbXBsZSBub25jZQ==", Some("mcp"));
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(response.contains("Sec-WebSocket-Protocol: mcp\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
        let request = b"GET / HTTP/1.1\r\nHost: x\r\nX-Token:  abc \r\n\r\nrest";
        let mut reader = &request[..];
        let headers = read_request(&mut reader).unwrap();
        assert_eq!(headers["x-token"], "abc");
        assert_eq!(reader, b"rest", "nothing past the headers is read");
    }

    #[test]
    fn masked_and_split_messages_arrive_whole_with_pings_answered() {
        let frame = |first: u8, payload: &[u8]| {
            let mask = [1, 2, 3, 4];
            let mut out = vec![first, 0x80 | payload.len() as u8];
            out.extend_from_slice(&mask);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            out
        };
        // "hel", a ping, "lo": one message, and the ping's payload sent back.
        let mut input = frame(0x01, b"hel");
        input.extend(frame(0x89, b"hi"));
        input.extend(frame(0x80, b"lo"));
        input.extend(frame(0x88, b""));
        let mut reader = &input[..];
        let mut pongs = Vec::new();
        let first = read_message(&mut reader, |p| pongs.push(p.to_vec())).unwrap();
        assert!(matches!(first, Message::Text(t) if t == "hello"));
        assert_eq!(pongs, [b"hi".to_vec()]);
        assert!(matches!(
            read_message(&mut reader, |_| {}),
            Ok(Message::Close)
        ));
        // Lengths past 125 take two more bytes.
        let mut out = Vec::new();
        write_text(&mut out, &"x".repeat(300)).unwrap();
        assert_eq!(&out[..4], &[0x81, 126, 1, 44]);
    }
}
