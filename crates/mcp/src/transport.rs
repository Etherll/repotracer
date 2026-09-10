//! Concurrent JSON-RPC transport for the MCP stdio endpoint.
//!
//! Input remains sequential because a byte stream has one framing cursor.
//! Request work is independent after a complete JSON value is read. A single
//! writer task owns the output stream, so responses can never interleave.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, error};

/// Keep model work bounded while the reader remains available for cancellation.
pub(crate) const DEFAULT_MAX_IN_FLIGHT: usize = 16;
const MAX_PENDING: usize = DEFAULT_MAX_IN_FLIGHT;
const RESPONSE_BUFFER: usize = DEFAULT_MAX_IN_FLIGHT;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

type HandlerFuture = Pin<Box<dyn Future<Output = Option<Value>> + Send + 'static>>;
type Handler = Arc<dyn Fn(Value) -> HandlerFuture + Send + Sync + 'static>;

#[derive(Clone, Debug, Eq)]
enum RequestId {
    String(String),
    Number(serde_json::Number),
}

impl PartialEq for RequestId {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Number(left), Self::Number(right)) => left == right,
            _ => false,
        }
    }
}

impl Hash for RequestId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::String(value) => value.hash(state),
            Self::Number(value) => value.hash(state),
        }
    }
}

impl RequestId {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(value) => Some(Self::String(value.clone())),
            Value::Number(value) => Some(Self::Number(value.clone())),
            _ => None,
        }
    }
}

struct PendingRequest {
    message: Value,
    id: Option<RequestId>,
    token: u64,
}

enum RequestState {
    Pending(u64),
    Active {
        token: u64,
        cancel: oneshot::Sender<()>,
    },
}

/// Serve an async byte stream with concurrent request handling.
///
/// The reader is consumed in wire order. Normal work uses a bounded active set
/// and pending queue; cancellation is handled by the dispatcher itself. The
/// writer is the only task touching `writer`. Once EOF is observed, no new work
/// is accepted and all accepted work is joined before the response channel
/// closes.
pub(crate) async fn serve<R, W, F, Fut>(reader: R, writer: W, handler: F) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(Value) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Option<Value>> + Send + 'static,
{
    let handler: Handler = Arc::new(move |message| Box::pin(handler(message)));
    serve_with_handler(reader, writer, handler).await
}

async fn serve_with_handler<R, W>(reader: R, writer: W, handler: Handler) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let (response_tx, response_rx) = mpsc::channel(RESPONSE_BUFFER);
    let mut writer_task = tokio::spawn(write_loop(writer, response_rx));
    let mut requests: JoinSet<(Option<RequestId>, u64)> = JoinSet::new();
    let mut pending = VecDeque::new();
    let mut registry = HashMap::new();
    let mut next_token = 0_u64;
    let mut request_join_error = None;
    let mut input_error = None;
    // Keep the framing future alive while request completions are selected.
    // `read_message` owns its partial line/header/payload buffers; dropping it
    // after `fill_buf` or `read_exact` has consumed bytes would lose that
    // prefix and desynchronize the next frame.
    let mut read_future = Box::pin(read_message(&mut reader));

    loop {
        reap_finished_requests(&mut requests, &mut registry, &mut request_join_error);
        if let Some(error) = request_join_error.take() {
            drop(response_tx);
            requests.abort_all();
            while requests.join_next().await.is_some() {}
            return Err(error);
        }

        while requests.len() < DEFAULT_MAX_IN_FLIGHT {
            let Some(request) = pending.pop_front() else {
                break;
            };
            start_request(
                request,
                &handler,
                &response_tx,
                &mut requests,
                &mut registry,
                &mut writer_task,
            )
            .await?;
        }

        let message = tokio::select! {
            result = &mut read_future => {
                match result {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(error) => {
                        error!("mcp read error: {error}");
                        input_error = Some(error);
                        break;
                    }
                }
            }
            result = requests.join_next(), if !requests.is_empty() => {
                if let Some(result) = result {
                    finish_request(result, &mut registry, &mut request_join_error);
                }
                continue;
            }
            result = &mut writer_task => {
                let error = writer_task_error(result);
                drop(response_tx);
                requests.abort_all();
                while requests.join_next().await.is_some() {}
                return Err(error);
            }
        };

        debug!(?message, "mcp message");
        if message.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            cancel_request(&message, &mut pending, &mut registry);
            drop(read_future);
            read_future = Box::pin(read_message(&mut reader));
            continue;
        }

        next_token = next_token.wrapping_add(1);
        let id = message.get("id").and_then(RequestId::from_value);
        if let Some(request_id) = id.as_ref() {
            // JSON-RPC request IDs must identify one outstanding request. If
            // two active/queued requests share an ID, a cancellation could
            // otherwise replace the registry entry for the first and leave
            // it running forever while cancelling only the second.
            if registry.contains_key(request_id) {
                let wire_id = message.get("id").cloned().unwrap_or(Value::Null);
                send_control_response(
                    &response_tx,
                    &writer_task,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": wire_id,
                        "error": {
                            "code": -32600,
                            "message": "RepoTracer request id is already in flight"
                        }
                    }),
                )?;
                drop(read_future);
                read_future = Box::pin(read_message(&mut reader));
                continue;
            }
        }
        let request = PendingRequest {
            message,
            id: id.clone(),
            token: next_token,
        };
        if requests.len() < DEFAULT_MAX_IN_FLIGHT {
            start_request(
                request,
                &handler,
                &response_tx,
                &mut requests,
                &mut registry,
                &mut writer_task,
            )
            .await?;
        } else if pending.len() < MAX_PENDING {
            if let Some(id) = id {
                registry.insert(id, RequestState::Pending(next_token));
            }
            pending.push_back(request);
        } else if let Some(id) = request.message.get("id").filter(|id| !id.is_null()) {
            // Continue reading so cancellation is never stuck behind a full
            // bounded queue. Overflow work is rejected before model startup.
            send_control_response(
                &response_tx,
                &writer_task,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32000, "message": "RepoTracer request queue is full"}
                }),
            )?;
        }
        drop(read_future);
        read_future = Box::pin(read_message(&mut reader));
    }

    let mut writer_finished = false;
    while !requests.is_empty() || !pending.is_empty() {
        while requests.len() < DEFAULT_MAX_IN_FLIGHT {
            let Some(request) = pending.pop_front() else {
                break;
            };
            start_request(
                request,
                &handler,
                &response_tx,
                &mut requests,
                &mut registry,
                &mut writer_task,
            )
            .await?;
        }
        tokio::select! {
            result = requests.join_next() => {
                if let Some(result) = result {
                    finish_request(result, &mut registry, &mut request_join_error);
                }
            }
            result = &mut writer_task, if !writer_finished => {
                match result {
                    Ok(Ok(())) => writer_finished = true,
                    result => {
                        let error = writer_task_error(result);
                        requests.abort_all();
                        while requests.join_next().await.is_some() {}
                        return Err(error);
                    }
                }
            }
        }
    }

    // Request tasks own sender clones, so close the writer only after EOF has
    // drained all accepted active and queued work.
    drop(response_tx);
    if !writer_finished {
        let writer_result = writer_task
            .await
            .context("MCP writer task stopped unexpectedly")?;
        writer_result?;
    }
    if let Some(error) = request_join_error {
        return Err(error);
    }

    // Preserve the old endpoint behavior: malformed input is logged and ends
    // the stream after accepted requests finish, rather than producing a
    // protocol response without a request id.
    let _ = input_error;
    Ok(())
}

async fn write_loop<W>(mut writer: W, mut responses: mpsc::Receiver<Value>) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(response) = responses.recv().await {
        let mut encoded = serde_json::to_vec(&response).context("serialize MCP response")?;
        encoded.push(b'\n');
        writer
            .write_all(&encoded)
            .await
            .context("write MCP response")?;
        writer.flush().await.context("flush MCP response")?;
    }
    Ok(())
}

fn send_control_response(
    sender: &mpsc::Sender<Value>,
    writer: &tokio::task::JoinHandle<Result<()>>,
    response: Value,
) -> Result<()> {
    // The input loop must not wait on a client that has stopped reading.
    // If rejection responses fill the bounded buffer, close this connection.
    // Returning drops the JoinSet and cancels its native work as well.
    sender.try_send(response).map_err(|error| {
        writer.abort();
        anyhow!("MCP response buffer unavailable; closing connection: {error}")
    })
}

fn writer_task_error(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow!("MCP writer stopped before input EOF"),
        Ok(Err(error)) => error,
        Err(error) => anyhow!("MCP writer task failed: {error}"),
    }
}

fn reap_finished_requests(
    requests: &mut JoinSet<(Option<RequestId>, u64)>,
    registry: &mut HashMap<RequestId, RequestState>,
    request_join_error: &mut Option<anyhow::Error>,
) {
    while let Some(result) = requests.try_join_next() {
        finish_request(result, registry, request_join_error);
    }
}

fn finish_request(
    result: std::result::Result<(Option<RequestId>, u64), tokio::task::JoinError>,
    registry: &mut HashMap<RequestId, RequestState>,
    request_join_error: &mut Option<anyhow::Error>,
) {
    match result {
        Ok((Some(id), token)) => {
            let is_current = registry.get(&id).is_some_and(|state| match state {
                RequestState::Pending(current) => *current == token,
                RequestState::Active { token: current, .. } => *current == token,
            });
            if is_current {
                registry.remove(&id);
            }
        }
        Ok((None, _)) => {}
        Err(error) => {
            request_join_error.get_or_insert_with(|| anyhow!("MCP request task failed: {error}"));
        }
    }
}

async fn start_request(
    request: PendingRequest,
    handler: &Handler,
    response_tx: &mpsc::Sender<Value>,
    requests: &mut JoinSet<(Option<RequestId>, u64)>,
    registry: &mut HashMap<RequestId, RequestState>,
    writer_task: &mut tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let PendingRequest { message, id, token } = request;
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let keep_cancel_open = if let Some(id) = id.clone() {
        registry.insert(
            id,
            RequestState::Active {
                token,
                cancel: cancel_tx,
            },
        );
        None
    } else {
        Some(cancel_tx)
    };
    let (started_tx, started_rx) = oneshot::channel();
    let response_tx = response_tx.clone();
    let handler = handler.clone();
    let task_id = id.clone();
    requests.spawn(async move {
        let _keep_cancel_open = keep_cancel_open;
        let mut future = handler(message);
        // Establish conversation-gate order before accepting another frame.
        let first_poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await;
        let _ = started_tx.send(());
        let response = match first_poll {
            std::task::Poll::Ready(response) => response,
            std::task::Poll::Pending => tokio::select! {
                biased;
                _ = &mut cancel_rx => None,
                response = future => response,
            },
        };
        if let Some(response) = response {
            tokio::select! {
                biased;
                _ = &mut cancel_rx => {}
                _ = response_tx.send(response) => {}
            }
        }
        (task_id, token)
    });

    let started = tokio::select! {
        result = started_rx => result,
        result = &mut *writer_task => return Err(writer_task_error(result)),
    };
    if started.is_err() {
        return Err(anyhow!("MCP request task stopped before dispatch"));
    }
    Ok(())
}

fn cancel_request(
    message: &Value,
    pending: &mut VecDeque<PendingRequest>,
    registry: &mut HashMap<RequestId, RequestState>,
) {
    let Some(id) = message
        .get("params")
        .and_then(|params| params.get("requestId"))
        .and_then(RequestId::from_value)
    else {
        return;
    };

    match registry.remove(&id) {
        Some(RequestState::Active { cancel, .. }) => {
            let _ = cancel.send(());
        }
        Some(RequestState::Pending(token)) => {
            if let Some(index) = pending
                .iter()
                .position(|request| request.token == token && request.id.as_ref() == Some(&id))
            {
                pending.remove(index);
            }
        }
        None => {}
    }
}

/// Read one newline-delimited or `Content-Length`-framed JSON value.
///
/// This mirrors the legacy synchronous parser in `lib.rs`. The transport uses
/// Tokio's buffered reader so a slow request never blocks a runtime worker.
async fn read_message<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: AsyncBufRead + Unpin,
{
    let first = loop {
        let Some(first) = read_bounded_line(reader).await? else {
            return Ok(None);
        };
        let first_trim = first.trim_end_matches(['\r', '\n']).to_owned();
        if !first_trim.is_empty() {
            break (first, first_trim);
        }
    };
    let (first, first_trim) = first;

    if first_trim
        .to_ascii_lowercase()
        .starts_with("content-length:")
    {
        let mut headers = first;
        loop {
            let Some(line) = read_bounded_line(reader).await? else {
                return Ok(None);
            };
            headers.push_str(&line);
            if headers.len() > MAX_FRAME_BYTES {
                return Err(anyhow!("MCP headers exceed {MAX_FRAME_BYTES} bytes"));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }
        let len = headers
            .lines()
            .find_map(|line| {
                let line = line.trim();
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .ok_or_else(|| anyhow!("missing Content-Length"))?;
        if len > MAX_FRAME_BYTES {
            return Err(anyhow!("MCP frame exceeds {MAX_FRAME_BYTES} bytes"));
        }
        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes).await?;
        return Ok(Some(serde_json::from_slice(&bytes)?));
    }

    Ok(Some(serde_json::from_str(&first_trim)?))
}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let (take, complete) = {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                if bytes.is_empty() {
                    return Ok(None);
                }
                (0, true)
            } else if let Some(index) = chunk.iter().position(|byte| *byte == b'\n') {
                (index + 1, true)
            } else {
                (chunk.len(), false)
            }
        };
        if take > 0 {
            if bytes.len() + take > MAX_FRAME_BYTES {
                return Err(anyhow!("MCP frame line exceeds {MAX_FRAME_BYTES} bytes"));
            }
            let chunk = reader.fill_buf().await?;
            bytes.extend_from_slice(&chunk[..take]);
            reader.consume(take);
        }
        if complete {
            return Ok(Some(String::from_utf8(bytes)?));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServer;
    use repotracer_core::{ScoutBackend, ScoutRequest, ScoutResult};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::duplex;
    use tokio::sync::Notify;
    use tokio::time::{sleep, timeout};

    fn request(id: usize) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#) + "\n"
    }

    #[tokio::test]
    async fn rejected_request_flood_cannot_block_cancellation_cleanup() {
        for duplicate in [true, false] {
            let dropped = Arc::new(AtomicBool::new(false));
            let entered = Arc::new(Notify::new());
            let observed = dropped.clone();
            let signal = entered.clone();
            let (mut input, reader) = duplex(64 * 1024);
            let (writer, _unread_output) = duplex(1);
            let server = tokio::spawn(serve(reader, writer, move |_message| {
                let dropped = observed.clone();
                let entered = signal.clone();
                async move {
                    let _guard = DropFlag(dropped);
                    entered.notify_one();
                    std::future::pending::<Option<Value>>().await
                }
            }));
            input.write_all(request(1).as_bytes()).await.unwrap();
            timeout(Duration::from_secs(1), entered.notified())
                .await
                .unwrap();
            for id in 2..100 {
                if input
                    .write_all(request(if duplicate { 1 } else { id }).as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let error = timeout(Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains("response buffer unavailable"));
            timeout(Duration::from_secs(1), async {
                while !dropped.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn responses_use_mcp_newline_framing() {
        let mut output = Vec::new();
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}))
            .await
            .unwrap();
        drop(sender);
        write_loop(&mut output, receiver).await.unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"id\":1,\"jsonrpc\":\"2.0\",\"result\":{}}\n"
        );
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected_before_reading_payload() {
        let header = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        assert!(read_message(&mut BufReader::new(header.as_bytes()))
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
        let bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(read_message(&mut BufReader::new(bytes.as_slice()))
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }

    #[tokio::test]
    async fn writer_failure_aborts_an_unfinished_request_after_eof() {
        struct OnDrop(Arc<AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = dropped.clone();
        let (mut input, reader) = duplex(4096);
        let (writer, output) = duplex(4096);
        drop(output);
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let observed = observed.clone();
            async move {
                if message["id"] == 1 {
                    let _guard = OnDrop(observed);
                    std::future::pending::<()>().await;
                }
                Some(json_response(&message))
            }
        }));
        input
            .write_all(format!("{}{}", request(1), request(2)).as_bytes())
            .await
            .unwrap();
        drop(input);
        assert!(timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_request_does_not_block_fast_request_or_ping() {
        let (mut input, reader) = duplex(4096);
        let (writer, mut output) = duplex(4096);
        let finished_slow = Arc::new(AtomicBool::new(false));
        let slow = finished_slow.clone();
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let slow = slow.clone();
            async move {
                match message["method"].as_str() {
                    Some("slow") => {
                        sleep(Duration::from_millis(100)).await;
                        slow.store(true, Ordering::SeqCst);
                    }
                    Some("fast") => {
                        assert!(!slow.load(Ordering::SeqCst));
                    }
                    _ => {}
                }
                Some(json_response(&message))
            }
        }));

        input
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"slow\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"fast\"}\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n",
            )
            .await
            .unwrap();
        drop(input);

        let mut responses = Vec::new();
        let mut output = BufReader::new(&mut output);
        for _ in 0..3 {
            let mut line = String::new();
            output.read_line(&mut line).await.unwrap();
            responses.push(serde_json::from_str::<Value>(line.trim()).unwrap());
        }
        assert_eq!(responses[0]["id"], 2);
        assert_eq!(responses[1]["id"], 3);
        assert_eq!(responses[2]["id"], 1);
        timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn eof_drains_accepted_work_and_keeps_records_complete() {
        let (mut input, reader) = duplex(4096);
        let (writer, mut output) = duplex(4096);
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let seen = seen.clone();
            async move {
                sleep(Duration::from_millis(10)).await;
                seen.fetch_add(1, Ordering::SeqCst);
                Some(json_response(&message))
            }
        }));
        for id in 0..8 {
            input.write_all(request(id).as_bytes()).await.unwrap();
        }
        drop(input);

        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap().unwrap();
        let lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        assert_eq!(lines.count(), 8);
        assert_eq!(count.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn content_length_input_keeps_legacy_framing() {
        let body = br#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = framed.into_bytes();
        bytes.extend_from_slice(body);
        let mut reader = BufReader::new(bytes.as_slice());
        let message = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(message["id"], 9);
        assert_eq!(message["method"], "ping");
    }

    struct SignalReader {
        inner: tokio::io::DuplexStream,
        reads: Arc<AtomicUsize>,
    }

    impl AsyncRead for SignalReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let result = Pin::new(&mut self.inner).poll_read(cx, buf);
            if let Poll::Ready(Ok(())) = &result {
                if buf.filled().len() > before {
                    self.reads.fetch_add(1, Ordering::SeqCst);
                }
            }
            result
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completing_request_during_partial_frame_does_not_drop_prefix() {
        let reads = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let entered = Arc::new(Notify::new());
        let (mut input, raw_reader) = duplex(4096);
        let reader = SignalReader {
            inner: raw_reader,
            reads: reads.clone(),
        };
        let (writer, raw_output) = duplex(4096);
        let task_release = release.clone();
        let task_entered = entered.clone();
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let release = task_release.clone();
            let entered = task_entered.clone();
            async move {
                if message["id"] == 1 {
                    entered.notify_one();
                    release.notified().await;
                }
                Some(json_response(&message))
            }
        }));
        let mut output = BufReader::new(raw_output);

        input.write_all(request(1).as_bytes()).await.unwrap();
        timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();

        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"")
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while reads.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();

        let mut first_response = String::new();
        timeout(
            Duration::from_secs(1),
            output.read_line(&mut first_response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(first_response.trim()).unwrap()["id"],
            1
        );

        input.write_all(b"}\n").await.unwrap();
        drop(input);
        let mut rest = Vec::new();
        output.read_to_end(&mut rest).await.unwrap();
        server.await.unwrap().unwrap();
        assert!(rest
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .any(|line| serde_json::from_slice::<Value>(line).unwrap()["id"] == 2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_frame_survives_request_completion_while_output_is_blocked() {
        let first_entered = Arc::new(Notify::new());
        let second_entered = Arc::new(Notify::new());
        let first_signal = first_entered.clone();
        let second_signal = second_entered.clone();
        let (mut input, reader) = duplex(16 * 1024);
        // Do not read output until the second request has been dispatched. A
        // response larger than this buffer leaves the writer blocked while
        // the framing future must retain the partial second line.
        let (writer, mut output) = duplex(1);
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let first_signal = first_signal.clone();
            let second_signal = second_signal.clone();
            async move {
                if message["id"] == 1 {
                    first_signal.notify_one();
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": "x".repeat(4096)
                    }))
                } else {
                    second_signal.notify_one();
                    Some(json_response(&message))
                }
            }
        }));

        input.write_all(request(1).as_bytes()).await.unwrap();
        timeout(Duration::from_secs(1), first_entered.notified())
            .await
            .unwrap();
        sleep(Duration::from_millis(20)).await;
        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"")
            .await
            .unwrap();
        sleep(Duration::from_millis(20)).await;
        input.write_all(b"}\n").await.unwrap();
        timeout(Duration::from_secs(1), second_entered.notified())
            .await
            .unwrap();
        drop(input);

        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap().unwrap();
        assert!(bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .any(|line| serde_json::from_slice::<Value>(line).unwrap()["id"] == 2));
    }

    fn cancellation(id: Value) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": id}
            })
        )
    }

    fn content_length_frame(body: &str) -> Vec<u8> {
        let body = body.trim_end();
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        frame
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn active_cancellation_drops_only_the_typed_target_and_connection_survives() {
        let numeric_dropped = Arc::new(AtomicBool::new(false));
        let observed = numeric_dropped.clone();
        let (mut input, reader) = duplex(16 * 1024);
        let (writer, mut output) = duplex(16 * 1024);
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let observed = observed.clone();
            async move {
                if message["id"] == serde_json::json!(7) {
                    let _drop = DropFlag(observed);
                    std::future::pending::<()>().await;
                }
                Some(json_response(&message))
            }
        }));

        input.write_all(request(7).as_bytes()).await.unwrap();
        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"7\",\"method\":\"ping\"}\n")
            .await
            .unwrap();
        input
            .write_all(&content_length_frame(&cancellation(serde_json::json!(7))))
            .await
            .unwrap();
        input.write_all(request(8).as_bytes()).await.unwrap();
        timeout(Duration::from_secs(1), async {
            while !numeric_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(input);

        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap().unwrap();
        let responses: Vec<Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert!(responses.iter().any(|response| response["id"] == "7"));
        assert!(responses.iter().any(|response| response["id"] == 8));
        assert!(!responses.iter().any(|response| response["id"] == 7));
    }

    #[tokio::test]
    async fn duplicate_in_flight_ids_are_rejected_without_stealing_cancellation() {
        let dropped = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(Notify::new());
        let observed = dropped.clone();
        let signaled = entered.clone();
        let (mut input, reader) = duplex(16 * 1024);
        let (writer, mut output) = duplex(16 * 1024);
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let observed = observed.clone();
            let signaled = signaled.clone();
            async move {
                if message["id"] == 1 {
                    signaled.notify_one();
                    let _drop = DropFlag(observed);
                    std::future::pending::<()>().await;
                }
                Some(json_response(&message))
            }
        }));

        input.write_all(request(1).as_bytes()).await.unwrap();
        timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        // The second request must not replace the first request's cancel
        // sender in the registry.
        input.write_all(request(1).as_bytes()).await.unwrap();
        input
            .write_all(cancellation(serde_json::json!(1)).as_bytes())
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        input.write_all(request(2).as_bytes()).await.unwrap();
        drop(input);

        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap().unwrap();
        let responses: Vec<Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert!(responses
            .iter()
            .any(|response| { response["id"] == 1 && response["error"]["code"] == -32600 }));
        assert!(responses.iter().any(|response| response["id"] == 2));
        assert_eq!(
            responses
                .iter()
                .filter(|response| response["id"] == 1)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn malformed_unknown_and_completed_cancellations_are_silent() {
        let (mut input, reader) = duplex(16 * 1024);
        let (writer, mut output) = duplex(16 * 1024);
        let server = tokio::spawn(serve(reader, writer, |message: Value| async move {
            Some(json_response(&message))
        }));
        input.write_all(request(1).as_bytes()).await.unwrap();
        for target in [
            Value::Null,
            serde_json::json!(true),
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!(404),
        ] {
            input
                .write_all(cancellation(target).as_bytes())
                .await
                .unwrap();
        }
        input
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":55,\"method\":\"notifications/cancelled\",\"params\":{}}\n",
            )
            .await
            .unwrap();
        input
            .write_all(cancellation(serde_json::json!(1)).as_bytes())
            .await
            .unwrap();
        input.write_all(request(2).as_bytes()).await.unwrap();
        drop(input);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap().unwrap();
        let ids: Vec<Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap()["id"].clone())
            .collect();
        assert_eq!(ids, vec![serde_json::json!(1), serde_json::json!(2)]);
    }

    #[tokio::test]
    async fn cancellation_is_read_at_full_capacity_and_queued_target_never_starts() {
        let entered = Arc::new(AtomicUsize::new(0));
        let cancelled_dropped = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let seen = entered.clone();
        let dropped = cancelled_dropped.clone();
        let unblock = release.clone();
        let (mut input, reader) = duplex(64 * 1024);
        let (writer, mut output) = duplex(64 * 1024);
        let server = tokio::spawn(serve(reader, writer, move |message: Value| {
            let seen = seen.clone();
            let dropped = dropped.clone();
            let unblock = unblock.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                let _drop = (message["id"] == 0).then(|| DropFlag(dropped));
                unblock.notified().await;
                Some(json_response(&message))
            }
        }));
        for id in 0..DEFAULT_MAX_IN_FLIGHT {
            input.write_all(request(id).as_bytes()).await.unwrap();
        }
        timeout(Duration::from_secs(1), async {
            while entered.load(Ordering::SeqCst) != DEFAULT_MAX_IN_FLIGHT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        input.write_all(request(99).as_bytes()).await.unwrap();
        input
            .write_all(cancellation(serde_json::json!(99)).as_bytes())
            .await
            .unwrap();
        input
            .write_all(cancellation(serde_json::json!(0)).as_bytes())
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !cancelled_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), DEFAULT_MAX_IN_FLIGHT);
        release.notify_waiters();
        drop(input);
        output.read_to_end(&mut Vec::new()).await.unwrap();
        server.await.unwrap().unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), DEFAULT_MAX_IN_FLIGHT);
    }

    #[derive(Clone)]
    struct OrderingScout {
        entered: Arc<Mutex<Vec<String>>>,
        slow_done: Arc<AtomicBool>,
        independent_beat_slow: Arc<AtomicBool>,
    }

    #[derive(Clone)]
    struct CancellationScout {
        entered: Arc<Mutex<Vec<String>>>,
        first_dropped: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ScoutBackend for CancellationScout {
        async fn scout(&self, request: ScoutRequest) -> anyhow::Result<ScoutResult> {
            self.entered.lock().unwrap().push(request.query.clone());
            if request.query == "first" {
                let _drop = DropFlag(self.first_dropped.clone());
                std::future::pending::<()>().await;
            }
            Ok(ScoutResult {
                summary: request.query,
                citations: Vec::new(),
                stats: Default::default(),
                investigation: Default::default(),
                raw_final: None,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_active_call_releases_conversation_for_queued_follow_up() {
        let entered = Arc::new(Mutex::new(Vec::new()));
        let first_dropped = Arc::new(AtomicBool::new(false));
        let root = tempfile::tempdir().unwrap();
        let mcp = McpServer::new(
            Arc::new(CancellationScout {
                entered: entered.clone(),
                first_dropped: first_dropped.clone(),
            }),
            root.path().to_path_buf(),
        );
        let (mut input, reader) = duplex(64 * 1024);
        let (writer, mut output) = duplex(64 * 1024);
        let serving = tokio::spawn(serve(reader, writer, move |message| {
            let mcp = mcp.clone();
            async move { mcp.handle_message(message).await }
        }));

        input
            .write_all(tool_call(1, "first", "same-handle").as_bytes())
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while entered.lock().unwrap().as_slice() != ["first"] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        input
            .write_all(tool_call(2, "second", "same-handle").as_bytes())
            .await
            .unwrap();
        input
            .write_all(cancellation(serde_json::json!(1)).as_bytes())
            .await
            .unwrap();

        timeout(Duration::from_secs(1), async {
            while entered.lock().unwrap().as_slice() != ["first", "second"] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(first_dropped.load(Ordering::SeqCst));
        drop(input);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        serving.await.unwrap().unwrap();
        let responses: Vec<Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 2);
    }

    #[async_trait::async_trait]
    impl ScoutBackend for OrderingScout {
        async fn scout(&self, request: ScoutRequest) -> anyhow::Result<ScoutResult> {
            self.entered.lock().unwrap().push(request.query.clone());
            if request.query == "same-1" {
                sleep(Duration::from_millis(100)).await;
                self.slow_done.store(true, Ordering::SeqCst);
            } else if request.query == "independent" && !self.slow_done.load(Ordering::SeqCst) {
                self.independent_beat_slow.store(true, Ordering::SeqCst);
            }
            Ok(ScoutResult {
                summary: request.query,
                citations: Vec::new(),
                stats: Default::default(),
                investigation: Default::default(),
                raw_final: None,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_keeps_same_conversation_fifo_while_independent_call_runs() {
        let entered = Arc::new(Mutex::new(Vec::new()));
        let slow_done = Arc::new(AtomicBool::new(false));
        let independent_beat_slow = Arc::new(AtomicBool::new(false));
        let backend = OrderingScout {
            entered: entered.clone(),
            slow_done,
            independent_beat_slow: independent_beat_slow.clone(),
        };
        let root = tempfile::tempdir().unwrap();
        let server = McpServer::new(Arc::new(backend), root.path().to_path_buf());
        let (mut input, reader) = duplex(64 * 1024);
        let (writer, mut output) = duplex(64 * 1024);
        let serving = tokio::spawn(serve(reader, writer, move |message| {
            let server = server.clone();
            async move { server.handle_message(message).await }
        }));

        input
            .write_all(
                format!(
                    "{}{}{}",
                    tool_call(1, "same-1", "same-handle"),
                    tool_call(3, "independent", "other-handle"),
                    tool_call(2, "same-2", "same-handle"),
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(input);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        serving.await.unwrap().unwrap();

        let entered = entered.lock().unwrap();
        assert_eq!(entered.len(), 3);
        // Repository selection can finish in either order across independent
        // handles. Only follow-ups on the same handle promise FIFO execution.
        assert_eq!(
            entered
                .iter()
                .filter(|query| query.starts_with("same-"))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["same-1", "same-2"]
        );
        assert!(independent_beat_slow.load(Ordering::SeqCst));
        assert_eq!(
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count(),
            3
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_conversation_waiter_never_starts_backend_work() {
        let entered = Arc::new(Mutex::new(Vec::new()));
        let backend = OrderingScout {
            entered: entered.clone(),
            slow_done: Arc::new(AtomicBool::new(false)),
            independent_beat_slow: Arc::new(AtomicBool::new(false)),
        };
        let root = tempfile::tempdir().unwrap();
        let mcp = McpServer::new(Arc::new(backend), root.path().to_path_buf());
        let (mut input, reader) = duplex(64 * 1024);
        let (writer, mut output) = duplex(64 * 1024);
        let serving = tokio::spawn(serve(reader, writer, move |message| {
            let mcp = mcp.clone();
            async move { mcp.handle_message(message).await }
        }));

        input
            .write_all(tool_call(1, "same-1", "same-handle").as_bytes())
            .await
            .unwrap();
        input
            .write_all(tool_call(2, "cancelled", "same-handle").as_bytes())
            .await
            .unwrap();
        input
            .write_all(cancellation(serde_json::json!(2)).as_bytes())
            .await
            .unwrap();
        input
            .write_all(tool_call(3, "same-3", "same-handle").as_bytes())
            .await
            .unwrap();
        drop(input);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        serving.await.unwrap().unwrap();

        assert_eq!(*entered.lock().unwrap(), vec!["same-1", "same-3"]);
        let ids: Vec<Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap()["id"].clone())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&serde_json::json!(1)));
        assert!(ids.contains(&serde_json::json!(3)));
    }

    fn tool_call(id: usize, query: &str, conversation_id: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "repo_scout",
                    "arguments": {
                        "query": query,
                        "investigation": {"conversation_id": conversation_id}
                    }
                }
            })
        )
    }

    fn json_response(message: &Value) -> Value {
        serde_json::json!({"jsonrpc":"2.0","id":message["id"],"result":{}})
    }
}
