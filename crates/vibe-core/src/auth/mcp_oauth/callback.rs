//! The loopback page the authorization server redirects the browser to.
//!
//! Reference `LoopbackCallbackHandler.serve_once`: bound on `127.0.0.1` at the
//! entry's `redirect_port` once the authorization URL is out, it answers the
//! first request that reaches it and closes. A request carrying a `code`
//! parameter gets the success page and ends the wait with that code and the
//! `state` beside it; anything else, including a connection that sends
//! nothing, gets the failure page with `400` and fails the login. Nothing
//! bounds the wait: the reference waits for the browser as long as it takes.
//! The pages are this port's own.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::flow::FlowError;

const SUCCESS_PAGE: &str = concat!(
    "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n",
    "<title>Vibe: signed in</title>\n</head>\n<body>\n<main>\n",
    "<h1>You are signed in</h1>\n",
    "<p>The MCP server has your authorization. This window can go; Vibe carries on in your terminal.</p>\n",
    "</main>\n</body>\n</html>\n"
);

const FAILURE_PAGE: &str = concat!(
    "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n",
    "<title>Vibe: sign-in incomplete</title>\n</head>\n<body>\n<main>\n",
    "<h1>Sign-in incomplete</h1>\n",
    "<p>No authorization code came back with this page. Go back to your terminal and start the sign-in again.</p>\n",
    "</main>\n</body>\n</html>\n"
);

/// Waits for the one callback of a login: the code and the state beside it.
pub(super) async fn serve_once(port: u16) -> Result<(String, Option<String>), FlowError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                FlowError::PortInUse(port)
            } else {
                FlowError::Io(error.to_string())
            }
        })?;
    // Connections are served concurrently, as `asyncio.start_server` serves
    // them, and the first one to finish decides the login.
    let (sender, mut outcomes) = mpsc::unbounded_channel();
    let mut handlers = Vec::new();
    let outcome = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let sender = sender.clone();
                handlers.push(tokio::spawn(async move {
                    let _ = sender.send(handle(stream).await);
                }));
            }
            Some(outcome) = outcomes.recv() => break outcome,
        }
    };
    drop(listener);
    // A connection still being read has nothing left to decide.
    for handler in handlers {
        handler.abort();
    }
    outcome
}

async fn handle(stream: TcpStream) -> Result<(String, Option<String>), FlowError> {
    let mut reader = BufReader::new(stream);
    let mut request_line = Vec::new();
    reader
        .read_until(b'\n', &mut request_line)
        .await
        .map_err(|error| FlowError::Io(error.to_string()))?;
    loop {
        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|error| FlowError::Io(error.to_string()))?;
        if matches!(line.as_slice(), b"\r\n" | b"\n" | b"") {
            break;
        }
    }
    let mut stream = reader.into_inner();
    let mut parts = request_line.splitn(3, |byte| *byte == b' ');
    let (_, target) = (parts.next(), parts.next());
    let Some(target) = target else {
        respond(&mut stream, false).await?;
        return Err(FlowError::Callback(
            "received a request it could not read".to_owned(),
        ));
    };
    // Latin-1, as the reference decodes the request target.
    let target = target
        .iter()
        .map(|byte| char::from(*byte))
        .collect::<String>();
    let query = super::flow::PyUrl::parse(&target).query;
    let first = |name: &str| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, value)| key == name && !value.is_empty())
            .map(|(_, value)| value.into_owned())
    };
    let Some(code) = first("code") else {
        respond(&mut stream, false).await?;
        return Err(FlowError::Callback(
            "arrived without a `code` parameter".to_owned(),
        ));
    };
    respond(&mut stream, true).await?;
    Ok((code, first("state")))
}

async fn respond(stream: &mut TcpStream, success: bool) -> Result<(), FlowError> {
    let (status, body) = if success {
        ("200 OK", SUCCESS_PAGE)
    } else {
        ("400 Bad Request", FAILURE_PAGE)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| FlowError::Io(error.to_string()))?;
    let _ = stream.shutdown().await;
    Ok(())
}
