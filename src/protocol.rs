pub const VERSION: u8 = 1;
pub const MAX_PAYLOAD: usize = 64 * 1024;
const HEADER: usize = 5;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Message {
    Attach { version: u8, rows: u16, cols: u16 },
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Disconnect { version: u8 },
    Output(Vec<u8>),
    Exit(i32),
    Detached,
    Error(String),
}

const ATTACH: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const DISCONNECT: u8 = 4;
const OUTPUT: u8 = 10;
const EXIT: u8 = 11;
const DETACHED: u8 = 12;
const ERROR: u8 = 13;

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload): (u8, Vec<u8>) = match self {
            Message::Attach {
                version,
                rows,
                cols,
            } => (
                ATTACH,
                [&[*version][..], &size_bytes(*rows, *cols)].concat(),
            ),
            Message::Input(data) => (INPUT, data.clone()),
            Message::Resize { rows, cols } => (RESIZE, size_bytes(*rows, *cols).to_vec()),
            Message::Disconnect { version } => (DISCONNECT, vec![*version]),
            Message::Output(data) => (OUTPUT, data.clone()),
            Message::Exit(code) => (EXIT, code.to_be_bytes().to_vec()),
            Message::Detached => (DETACHED, Vec::new()),
            Message::Error(text) => (ERROR, text.as_bytes().to_vec()),
        };
        let mut frame = Vec::with_capacity(HEADER + payload.len());
        frame.push(kind);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    fn decode(kind: u8, payload: &[u8]) -> Result<Message, String> {
        let invalid = || format!("malformed message of type {kind}");
        let message = match kind {
            ATTACH => match payload {
                [version, size @ ..] => {
                    let (rows, cols) = parse_size(size).ok_or_else(invalid)?;
                    Message::Attach {
                        version: *version,
                        rows,
                        cols,
                    }
                }
                [] => return Err(invalid()),
            },
            INPUT => Message::Input(payload.to_vec()),
            RESIZE => {
                let (rows, cols) = parse_size(payload).ok_or_else(invalid)?;
                Message::Resize { rows, cols }
            }
            DISCONNECT => match payload {
                [version] => Message::Disconnect { version: *version },
                _ => return Err(invalid()),
            },
            OUTPUT => Message::Output(payload.to_vec()),
            EXIT => Message::Exit(i32::from_be_bytes(
                payload.try_into().map_err(|_| invalid())?,
            )),
            DETACHED if payload.is_empty() => Message::Detached,
            ERROR => Message::Error(String::from_utf8_lossy(payload).into_owned()),
            _ => return Err(invalid()),
        };
        Ok(message)
    }
}

fn size_bytes(rows: u16, cols: u16) -> [u8; 4] {
    let [r0, r1] = rows.to_be_bytes();
    let [c0, c1] = cols.to_be_bytes();
    [r0, r1, c0, c1]
}

fn parse_size(bytes: &[u8]) -> Option<(u16, u16)> {
    match bytes {
        [r0, r1, c0, c1] => Some((
            u16::from_be_bytes([*r0, *r1]),
            u16::from_be_bytes([*c0, *c1]),
        )),
        _ => None,
    }
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn next_message(&mut self) -> Result<Option<Message>, String> {
        let Some(header) = self.buffer.get(..HEADER) else {
            return Ok(None);
        };
        let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD {
            return Err(format!("message of {length} bytes exceeds limit"));
        }
        if self.buffer.len() < HEADER + length {
            return Ok(None);
        }
        let message = Message::decode(header[0], &self.buffer[HEADER..HEADER + length]);
        self.buffer.drain(..HEADER + length);
        message.map(Some)
    }
}

pub fn chunks(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.chunks(MAX_PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds() -> Vec<Message> {
        vec![
            Message::Attach {
                version: VERSION,
                rows: 24,
                cols: 80,
            },
            Message::Input(b"ls\r".to_vec()),
            Message::Input(Vec::new()),
            Message::Resize {
                rows: 65535,
                cols: 1,
            },
            Message::Disconnect { version: 7 },
            Message::Output(vec![0, 0x1b, 0xff]),
            Message::Exit(-3),
            Message::Exit(130),
            Message::Detached,
            Message::Error("no such session".into()),
        ]
    }

    fn decode_all(decoder: &mut Decoder) -> Vec<Message> {
        std::iter::from_fn(|| decoder.next_message().unwrap()).collect()
    }

    #[test]
    fn round_trip() {
        for message in all_kinds() {
            let mut decoder = Decoder::default();
            decoder.push(&message.encode());
            assert_eq!(decoder.next_message(), Ok(Some(message)));
            assert_eq!(decoder.next_message(), Ok(None));
        }
    }

    #[test]
    fn concatenated_frames() {
        let stream: Vec<u8> = all_kinds().iter().flat_map(Message::encode).collect();
        let mut decoder = Decoder::default();
        decoder.push(&stream);
        assert_eq!(decode_all(&mut decoder), all_kinds());
    }

    #[test]
    fn byte_at_a_time() {
        let stream: Vec<u8> = all_kinds().iter().flat_map(Message::encode).collect();
        let mut decoder = Decoder::default();
        let mut decoded = Vec::new();
        for byte in stream {
            decoder.push(&[byte]);
            decoded.extend(decode_all(&mut decoder));
        }
        assert_eq!(decoded, all_kinds());
    }

    #[test]
    fn maximum_payload_accepted() {
        let message = Message::Output(vec![b'x'; MAX_PAYLOAD]);
        let mut decoder = Decoder::default();
        decoder.push(&message.encode());
        assert_eq!(decoder.next_message(), Ok(Some(message)));
    }

    #[test]
    fn oversize_rejected_from_header() {
        let mut decoder = Decoder::default();
        let mut header = vec![OUTPUT];
        header.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        decoder.push(&header);
        assert!(decoder.next_message().is_err());
    }

    #[test]
    fn malformed_rejected() {
        for frame in [
            vec![ATTACH, 0, 0, 0, 0],
            vec![ATTACH, 0, 0, 0, 2, 1, 0],
            vec![RESIZE, 0, 0, 0, 1, 0],
            vec![DISCONNECT, 0, 0, 0, 0],
            vec![EXIT, 0, 0, 0, 1, 0],
            vec![DETACHED, 0, 0, 0, 1, 0],
            vec![99, 0, 0, 0, 0],
        ] {
            let mut decoder = Decoder::default();
            decoder.push(&frame);
            assert!(decoder.next_message().is_err(), "{frame:?}");
        }
    }

    #[test]
    fn chunking_respects_limit() {
        let data = vec![0u8; MAX_PAYLOAD * 2 + 1];
        let sizes: Vec<usize> = chunks(&data).map(<[u8]>::len).collect();
        assert_eq!(sizes, [MAX_PAYLOAD, MAX_PAYLOAD, 1]);
    }
}
