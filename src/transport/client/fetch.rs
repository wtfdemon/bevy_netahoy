//! Asking the server for its offer: one plain GET, natively over a socket, in a browser with the
//! page's fetch.

use crate::transport::Offer;

/// How long a native ask may take in all, from its connect to the answer's last byte: under
/// [`ASK_WAIT_SECS`](crate::transport::rules::ASK_WAIT_SECS), so the IO thread is free again by
/// the time the client stops waiting.
#[cfg(not(target_family = "wasm"))]
const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The longest answer a native ask reads; the offer is a hundred bytes or so.
#[cfg(not(target_family = "wasm"))]
const MAX_ANSWER_BYTES: usize = 16 * 1024;

/// One plain GET on an IO thread, which it blocks for at most [`ASK_TIMEOUT`] once it has the
/// server's address (looking up a host name can take longer; an IP takes no time).
#[cfg(not(target_family = "wasm"))]
pub(in crate::transport) async fn fetch_offer(url: String) -> Result<Offer, String> {
    use std::{
        io::{Read, Write},
        net::{TcpStream, ToSocketAddrs},
        time::Instant,
    };
    let failed = |why: &dyn std::fmt::Display| format!("{url}: {why}");
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| failed(&"not an http:// url"))?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
    let addr = authority
        .to_socket_addrs()
        .map_err(|err| failed(&err))?
        .next()
        .ok_or_else(|| failed(&"no address"))?;
    // One deadline for the whole request: a timeout per read alone would let an answer that
    // trickles in a byte at a time hold the thread for good.
    let deadline = Instant::now() + ASK_TIMEOUT;
    let left = || {
        Some(deadline.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .ok_or_else(|| failed(&format!("no answer in {} s", ASK_TIMEOUT.as_secs())))
    };
    let mut stream = TcpStream::connect_timeout(&addr, left()?).map_err(|err| failed(&err))?;
    stream
        .set_write_timeout(Some(left()?))
        .map_err(|err| failed(&err))?;
    // One write: the server answers once the request is in, and a request in pieces could
    // still be arriving when it closes.
    let request = format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|err| failed(&err))?;
    let mut response = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|err| failed(&err))?;
        match stream.read(&mut chunk).map_err(|err| failed(&err))? {
            0 => break,
            read => response.extend_from_slice(&chunk[..read]),
        }
        if response.len() > MAX_ANSWER_BYTES {
            return Err(failed(&format!("an answer over {MAX_ANSWER_BYTES} bytes")));
        }
    }
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    match head.split_whitespace().nth(1) {
        Some("200") => serde_json::from_str(body).map_err(|err| failed(&err)),
        status => Err(failed(&format!("status {}", status.unwrap_or("missing")))),
    }
}

#[cfg(target_family = "wasm")]
pub(in crate::transport) async fn fetch_offer(url: String) -> Result<Offer, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    let failed = |why: wasm_bindgen::JsValue| format!("{url}: {why:?}");
    let window = web_sys::window().ok_or_else(|| format!("{url}: no window"))?;
    let response: web_sys::Response = JsFuture::from(window.fetch_with_str(&url))
        .await
        .map_err(failed)?
        .dyn_into()
        .map_err(failed)?;
    if !response.ok() {
        return Err(format!("{url}: status {}", response.status()));
    }
    let text = JsFuture::from(response.text().map_err(failed)?)
        .await
        .map_err(failed)?;
    serde_json::from_str(&text.as_string().unwrap_or_default())
        .map_err(|err| format!("{url}: {err}"))
}
