//! TFTP packet encoding and decoding.

use std::fmt;

const RRQ: u16 = 1;
const WRQ: u16 = 2;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;
const OACK: u16 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NotDefined = 0,
    FileNotFound = 1,
    AccessViolation = 2,
    IllegalOperation = 4,
    UnknownTid = 5,
    OptionRefused = 8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet<'a> {
    Read { path: String, mode: String, options: Vec<(String, String)> },
    Write { path: String },
    Data { block: u16, data: &'a [u8] },
    Ack { block: u16 },
    Error { code: u16, message: String },
    OptionAck { options: Vec<(String, String)> },
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(&'static str);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ParseError {}

fn strings(mut b: &[u8]) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let end = b.iter().position(|&c| c == 0).ok_or(ParseError("unterminated string"))?;
        out.push(String::from_utf8_lossy(&b[..end]).into_owned());
        b = &b[end + 1..];
    }
    Ok(out)
}

fn pairs(fields: &[String]) -> Vec<(String, String)> {
    fields.as_chunks::<2>().0.iter().map(|[k, v]| (k.to_ascii_lowercase(), v.clone())).collect()
}

impl<'a> Packet<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Packet<'a>, ParseError> {
        if b.len() < 4 {
            return Err(ParseError("short packet"));
        }
        let op = u16::from_be_bytes([b[0], b[1]]);
        let rest = &b[2..];
        let num = || u16::from_be_bytes([rest[0], rest[1]]);
        match op {
            RRQ | WRQ => {
                let f = strings(rest)?;
                if f.len() < 2 {
                    return Err(ParseError("request without path and mode"));
                }
                if op == WRQ {
                    return Ok(Packet::Write { path: f[0].clone() });
                }
                Ok(Packet::Read { path: f[0].clone(), mode: f[1].clone(), options: pairs(&f[2..]) })
            }
            DATA => Ok(Packet::Data { block: num(), data: &rest[2..] }),
            ACK => Ok(Packet::Ack { block: num() }),
            ERROR => {
                let msg = strings(&rest[2..]).ok().and_then(|v| v.into_iter().next()).unwrap_or_default();
                Ok(Packet::Error { code: num(), message: msg })
            }
            OACK => Ok(Packet::OptionAck { options: pairs(&strings(rest)?) }),
            _ => Err(ParseError("unknown opcode")),
        }
    }

    pub fn error(code: ErrorCode, message: &str) -> Vec<u8> {
        let mut v = Vec::with_capacity(5 + message.len());
        v.extend_from_slice(&ERROR.to_be_bytes());
        v.extend_from_slice(&(code as u16).to_be_bytes());
        v.extend_from_slice(message.as_bytes());
        v.push(0);
        v
    }

    pub fn data(block: u16, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + data.len());
        v.extend_from_slice(&DATA.to_be_bytes());
        v.extend_from_slice(&block.to_be_bytes());
        v.extend_from_slice(data);
        v
    }

    pub fn ack(block: u16) -> Vec<u8> {
        let mut v = ACK.to_be_bytes().to_vec();
        v.extend_from_slice(&block.to_be_bytes());
        v
    }

    pub fn oack(options: &[(String, String)]) -> Vec<u8> {
        let mut v = OACK.to_be_bytes().to_vec();
        for (k, val) in options {
            v.extend_from_slice(k.as_bytes());
            v.push(0);
            v.extend_from_slice(val.as_bytes());
            v.push(0);
        }
        v
    }

    /// A read request, for clients and tests.
    pub fn rrq(path: &str, options: &[(&str, &str)]) -> Vec<u8> {
        let mut v = RRQ.to_be_bytes().to_vec();
        for s in [path, "octet"].into_iter().chain(options.iter().flat_map(|(k, val)| [*k, *val])) {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v
    }
}
