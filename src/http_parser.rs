use bytes::{Buf, BytesMut};
use httparse::{EMPTY_HEADER, Request};
use std::error::Error;

#[derive(Debug)]
pub struct ParsedRequest {
    pub method: String,
    pub path: String,
    pub version: u8,
    pub headers: Vec<(String, String)>,
    // pub body: String,
}

pub fn parse_http_request(
    buffer: &mut BytesMut,
) -> Result<Option<ParsedRequest>, Box<dyn Error + Send + Sync>> {
    let mut headers = [EMPTY_HEADER; 16];
    let mut req = Request::new(&mut headers);

    let status = req.parse(buffer)?;

    if status.is_complete() {
        let parsed_len = status.unwrap();

        let parsed_req = ParsedRequest {
            method: req.method.unwrap_or("").to_string(),
            path: req.path.unwrap_or("").to_string(),
            version: req.version.unwrap_or(0),
            headers: headers
                .iter()
                .take_while(|h| h.name != "")
                .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned()))
                .collect(),
        };
        buffer.advance(parsed_len);
        Ok(Some(parsed_req))
    } else {
        Ok(None)
    }
}
