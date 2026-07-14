//! Bounded HTTP request reader for deterministic loopback fixtures.

use std::io::{self, Read};

const MAX_FIXTURE_HEADERS: usize = 64 * 1024;
const MAX_FIXTURE_BODY: usize = 2 * 1024 * 1024;

/// One bounded HTTP request consumed by a loopback fixture.
#[derive(Debug)]
pub(crate) struct ScriptedHttpRequest {
    /// Raw request line including its CRLF terminator.
    pub(crate) request_line: Vec<u8>,
    /// Exact request body selected by the sole Content-Length header.
    pub(crate) body: Vec<u8>,
}

/// Reads one complete bounded HTTP/1 request with a mandatory Content-Length.
pub(crate) fn read_bounded_http_request(reader: &mut impl Read) -> io::Result<ScriptedHttpRequest> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        if headers.len() == MAX_FIXTURE_HEADERS {
            return Err(invalid_data("fixture HTTP headers exceed limit"));
        }
        reader.read_exact(&mut byte)?;
        headers.push(byte[0]);
    }
    let header_text = std::str::from_utf8(&headers)
        .map_err(|_| invalid_data("fixture HTTP headers are not UTF-8"))?;
    let request_line_end = headers
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| invalid_data("fixture HTTP request line is missing"))?
        + 2;
    let mut content_length = None;
    for line in header_text
        .split("\r\n")
        .skip(1)
        .filter(|line| !line.is_empty())
    {
        let Some((name, value)) = line.split_once(':') else {
            return Err(invalid_data("fixture HTTP header has no colon"));
        };
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if content_length.is_some() {
            return Err(invalid_data("fixture HTTP has duplicate Content-Length"));
        }
        let parsed = value
            .trim_matches([' ', '\t'])
            .parse::<usize>()
            .map_err(|_| invalid_data("fixture HTTP Content-Length is invalid"))?;
        if MAX_FIXTURE_BODY < parsed {
            return Err(invalid_data("fixture HTTP body exceeds limit"));
        }
        content_length = Some(parsed);
    }
    let content_length =
        content_length.ok_or_else(|| invalid_data("fixture HTTP Content-Length is missing"))?;
    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(ScriptedHttpRequest {
        request_line: headers[..request_line_end].to_vec(),
        body,
    })
}

/// Creates a consistently classified fixture parser error.
fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
