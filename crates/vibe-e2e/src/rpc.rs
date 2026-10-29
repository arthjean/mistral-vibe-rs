//! A JSON-RPC peer over a child's stdio, as an editor or an app-server client
//! speaks to `vibe-acp` and the app server: one message per line.
//!
//! Requests are correlated by id. Notifications and responses nobody took yet
//! wait in an inbox, so a scenario reads them in any order. A request the child
//! sends is answered by the first responder registered for its method, and the
//! exchange is journaled; one no responder covers waits for the scenario.
//! Raw mode writes bytes unframed and reads lines unparsed, for wire tests.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::process::{Child, Output};
use crate::{DEFAULT_DEADLINE, Failure, Outcome, scaled};

/// Ids of the requests a responder sends after its answer, kept apart from the
/// scenario's numeric ids.
const FOLLOW_UP_PREFIX: &str = "e2e-client-";

/// What a responder answers a request with, and the requests it sends next
/// (such as the `callback/result` that completes a `callback/call`).
#[derive(Debug, Clone)]
pub struct Answer {
    reply: Result<Value, Value>,
    then: Vec<(String, Value)>,
}

impl Answer {
    pub fn result(result: Value) -> Self {
        Self {
            reply: Ok(result),
            then: Vec::new(),
        }
    }

    pub fn error(code: i64, message: &str) -> Self {
        Self {
            reply: Err(json!({"code": code, "message": message})),
            then: Vec::new(),
        }
    }

    /// A request sent once the answer is written.
    #[must_use]
    pub fn then(mut self, method: &str, params: Value) -> Self {
        self.then.push((method.to_owned(), params));
        self
    }
}

/// One request the child sent and a responder answered.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub method: String,
    pub params: Value,
    /// The response message written back.
    pub answer: Value,
    /// The ids of the follow-up requests sent after the answer.
    pub follow_up_ids: Vec<String>,
    /// The responses to the follow-up requests, as they arrive.
    pub follow_ups: Vec<Value>,
}

type Responder = Box<dyn FnMut(&Value) -> Answer>;

/// The client side of a JSON-RPC connection to a child.
pub struct Rpc {
    child: Child,
    next_id: u64,
    next_follow_up: u64,
    deadline: Duration,
    inbox: VecDeque<Value>,
    requests: VecDeque<Value>,
    notifications: Vec<Value>,
    responders: Vec<(String, Responder)>,
    exchanges: Vec<Exchange>,
}

impl Rpc {
    /// Speaks to `child`, whose stdin must be piped.
    pub fn new(child: Child) -> Self {
        Self {
            child,
            next_id: 0,
            next_follow_up: 0,
            deadline: DEFAULT_DEADLINE,
            inbox: VecDeque::new(),
            requests: VecDeque::new(),
            notifications: Vec::new(),
            responders: Vec::new(),
            exchanges: Vec::new(),
        }
    }

    /// How long a wait lasts when it names no deadline, before [`scaled`].
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Answers every request for `method` (or every method under a prefix
    /// written `terminal/*`) with what `responder` returns. An earlier
    /// registration wins.
    pub fn respond(&mut self, method: &str, responder: impl FnMut(&Value) -> Answer + 'static) {
        self.responders
            .push((method.to_owned(), Box::new(responder)));
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn into_child(self) -> Child {
        self.child
    }

    /// Closes the client's end, as a client that disconnects does, and waits
    /// for the child to exit.
    pub fn hang_up(self) -> Outcome<Output> {
        let mut child = self.child;
        child.close_stdin();
        child.wait_within(Duration::from_secs(15))
    }

    /// Every notification received so far, in arrival order, taken or not.
    pub fn notifications(&self) -> &[Value] {
        &self.notifications
    }

    /// Every request a responder answered, in arrival order.
    pub fn exchanges(&self) -> &[Exchange] {
        &self.exchanges
    }

    /// Writes one message as a line.
    pub fn write(&mut self, message: &Value) -> Outcome {
        self.child.write_line(&message.to_string())
    }

    /// Sends a request and returns its id without waiting.
    pub fn send(&mut self, method: &str, params: Value) -> Outcome<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Outcome {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// The response to `id`, whole: a `result` or an `error` member.
    pub fn response(&mut self, id: u64) -> Outcome<Value> {
        self.response_within(id, self.deadline)
    }

    pub fn response_within(&mut self, id: u64, deadline: Duration) -> Outcome<Value> {
        let what = format!("the response to request {id}");
        self.take(deadline, &what, |message| {
            message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id)
        })
    }

    /// Sends a request and waits for its whole response.
    pub fn request(&mut self, method: &str, params: Value) -> Outcome<Value> {
        let id = self.send(method, params)?;
        self.response(id)
    }

    /// Sends a request and returns its result, failing on an error response.
    pub fn call(&mut self, method: &str, params: Value) -> Outcome<Value> {
        let response = self.request(method, params)?;
        match response.get("result") {
            Some(result) if response.get("error").is_none() => Ok(result.clone()),
            _ => Err(Failure(format!("{method} answered {response}"))),
        }
    }

    /// The first notification not taken yet that satisfies `predicate`; the
    /// ones before it stay in the inbox.
    pub fn await_notification(
        &mut self,
        predicate: impl Fn(&Value) -> bool,
        deadline: Duration,
    ) -> Outcome<Value> {
        self.take(deadline, "a matching notification", |message| {
            message.get("method").is_some() && message.get("id").is_none() && predicate(message)
        })
    }

    /// The first notification for `method` not taken yet.
    pub fn await_method(&mut self, method: &str, deadline: Duration) -> Outcome<Value> {
        self.await_notification(|message| message["method"] == method, deadline)
    }

    /// The first request of the child that no responder answered and that
    /// satisfies `predicate`.
    pub fn await_request(
        &mut self,
        predicate: impl Fn(&Value) -> bool,
        deadline: Duration,
    ) -> Outcome<Value> {
        let until = Instant::now() + scaled(deadline);
        loop {
            if let Some(index) = self.requests.iter().position(&predicate) {
                return self
                    .requests
                    .remove(index)
                    .ok_or_else(|| Failure::new("the request vanished"));
            }
            self.pump(until, "a matching request from the child")?;
        }
    }

    /// Answers a request of the child by hand.
    pub fn reply(&mut self, request: &Value, result: Value) -> Outcome {
        self.write(&json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
    }

    /// Writes `bytes` as given, without a newline.
    pub fn write_raw(&mut self, bytes: impl AsRef<[u8]>) -> Outcome {
        self.child.write(bytes)
    }

    /// The next stdout line, unparsed, bypassing the inbox.
    pub fn raw_line(&mut self, deadline: Duration) -> Outcome<String> {
        self.child.next_line(deadline)
    }

    /// Removes and returns the first inbox message `matches` accepts, reading
    /// more until `deadline`.
    fn take(
        &mut self,
        deadline: Duration,
        what: &str,
        matches: impl Fn(&Value) -> bool,
    ) -> Outcome<Value> {
        let until = Instant::now() + scaled(deadline);
        loop {
            if let Some(index) = self.inbox.iter().position(&matches) {
                return self
                    .inbox
                    .remove(index)
                    .ok_or_else(|| Failure::new("the message vanished"));
            }
            self.pump(until, what)?;
        }
    }

    /// Reads and dispatches one message.
    fn pump(&mut self, until: Instant, what: &str) -> Outcome {
        let line = self
            .child
            .next_line_until(until)
            .map_err(|Failure(message)| Failure(format!("waiting for {what}: {message}")))?;
        if line.trim().is_empty() {
            return Ok(());
        }
        let message: Value = serde_json::from_str(&line).map_err(|error| {
            Failure(format!(
                "the child wrote a line that is not JSON ({error}): {line}"
            ))
        })?;
        let is_request = message.get("method").is_some() && message.get("id").is_some();
        let is_notification = message.get("method").is_some() && message.get("id").is_none();
        if is_request {
            return self.dispatch(message);
        }
        if is_notification {
            self.notifications.push(message.clone());
        } else if let Some(id) = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with(FOLLOW_UP_PREFIX))
        {
            let id = id.to_owned();
            if let Some(exchange) = self
                .exchanges
                .iter_mut()
                .find(|exchange| exchange.follow_up_ids.contains(&id))
            {
                exchange.follow_ups.push(message);
            }
            return Ok(());
        }
        self.inbox.push_back(message);
        Ok(())
    }

    /// Answers a request of the child from its responder, or keeps it.
    fn dispatch(&mut self, request: Value) -> Outcome {
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let Some((_, responder)) = self.responders.iter_mut().find(|(pattern, _)| {
            pattern == &method
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| method.starts_with(prefix))
        }) else {
            self.requests.push_back(request);
            return Ok(());
        };
        let answer = responder(&request["params"]);
        let response = match answer.reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": request["id"], "error": error}),
        };
        self.write(&response)?;
        let mut ids = Vec::new();
        for (follow_up, params) in answer.then {
            self.next_follow_up += 1;
            let id = format!("{FOLLOW_UP_PREFIX}{}", self.next_follow_up);
            self.write(
                &json!({"jsonrpc": "2.0", "id": id, "method": follow_up, "params": params}),
            )?;
            ids.push(id);
        }
        self.exchanges.push(Exchange {
            method,
            params: request["params"].clone(),
            answer: response,
            follow_up_ids: ids,
            follow_ups: Vec::new(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod rpc_tests;
