use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use item_mcp::transport::BoundedTransport;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, JsonRpcMessage},
    service::{RequestContext, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream},
    time::timeout,
};

const SHORT: Duration = Duration::from_millis(30);
const LONG: Duration = Duration::from_secs(3);

type Sdk = AsyncRwTransport<RoleServer, DuplexStream, DuplexStream>;
struct Counted {
    inner: Sdk,
    reads: Arc<AtomicUsize>,
}
impl Transport<RoleServer> for Counted {
    type Error = io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        self.inner.send(item)
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        let message = self.inner.receive().await;
        self.reads.fetch_add(1, Ordering::SeqCst);
        message
    }
    async fn close(&mut self) -> io::Result<()> {
        self.inner.close().await
    }
}
fn setup(
    deadline: Duration,
) -> (
    BoundedTransport<Counted>,
    DuplexStream,
    DuplexStream,
    Arc<AtomicUsize>,
) {
    let (input, host_in) = tokio::io::duplex(65536);
    let (output, host_out) = tokio::io::duplex(64);
    let reads = Arc::new(AtomicUsize::new(0));
    let inner = Counted {
        inner: AsyncRwTransport::new_server(input, output),
        reads: reads.clone(),
    };
    (
        BoundedTransport::with_write_timeout(inner, deadline),
        host_in,
        host_out,
        reads,
    )
}
async fn write(host: &mut DuplexStream, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    host.write_all(&bytes).await.unwrap();
}
fn request(id: impl Into<Value>) -> Value {
    json!({"jsonrpc":"2.0", "method":"ping", "id":id.into()})
}
fn cancel(id: impl Into<Value>) -> Value {
    json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":id.into()}})
}
fn response(id: impl Into<Value>) -> TxJsonRpcMessage<RoleServer> {
    serde_json::from_value(json!({"jsonrpc":"2.0", "id":id.into(), "result":{"content":[{"type":"text", "text":"x".repeat(16384)}]}})).unwrap()
}
async fn receive_id(transport: &mut impl Transport<RoleServer, Error = io::Error>) -> Value {
    let msg = timeout(LONG, transport.receive()).await.unwrap().unwrap();
    serde_json::to_value(msg).unwrap()["id"].clone()
}
async fn fill(
    transport: &mut impl Transport<RoleServer, Error = io::Error>,
    host: &mut DuplexStream,
) {
    for id in 1..=4 {
        write(host, request(id)).await;
        assert_eq!(receive_id(transport).await, id);
    }
}

#[tokio::test]
async fn slow_stdout_holds_budget_through_queued_and_actual_writes() {
    let (mut transport, mut host_in, host_out, reads) = setup(LONG);
    fill(&mut transport, &mut host_in).await;
    let mut tasks = Vec::new();
    for id in 1..=4 {
        tasks.push(tokio::spawn(transport.send(response(id))));
    }
    for id in 5..=30 {
        write(&mut host_in, request(id)).await;
    }
    for _ in 0..3 {
        assert!(timeout(SHORT, transport.receive()).await.is_err());
    }
    assert_eq!(
        reads.load(Ordering::SeqCst),
        5,
        "only one lookahead is retained across canceled receive futures"
    );
    assert!(tasks.iter().all(|task| !task.is_finished()));
    let mut host_out = BufReader::new(host_out);
    let mut line = String::new();
    timeout(LONG, host_out.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 1);
    tasks.remove(0).await.unwrap().unwrap();
    assert_eq!(receive_id(&mut transport).await, 5);
    assert!(timeout(SHORT, transport.receive()).await.is_err());
    for _ in 0..3 {
        line.clear();
        timeout(LONG, host_out.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    transport.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_and_notifications_do_not_leak_or_consume_slots() {
    let (mut transport, mut host_in, _host_out, _) = setup(LONG);
    fill(&mut transport, &mut host_in).await;
    for message in [
        cancel(999),
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
    ] {
        write(&mut host_in, message).await;
        assert!(matches!(
            transport.receive().await,
            Some(JsonRpcMessage::Notification(_))
        ));
    }
    write(&mut host_in, cancel(1)).await;
    assert!(matches!(
        transport.receive().await,
        Some(JsonRpcMessage::Notification(_))
    ));
    write(&mut host_in, request(5)).await;
    assert_eq!(receive_id(&mut transport).await, 5);
    write(&mut host_in, request(6)).await;
    assert!(timeout(SHORT, transport.receive()).await.is_err());
    transport.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_cannot_free_a_send_that_is_already_queued_or_writing() {
    let (mut transport, mut host_in, host_out, _) = setup(LONG);
    fill(&mut transport, &mut host_in).await;
    let send = tokio::spawn(transport.send(response(1)));
    write(&mut host_in, cancel(1)).await;
    assert!(matches!(
        transport.receive().await,
        Some(JsonRpcMessage::Notification(_))
    ));
    write(&mut host_in, request(5)).await;
    assert!(timeout(SHORT, transport.receive()).await.is_err());
    let mut host_out = BufReader::new(host_out);
    let mut line = String::new();
    timeout(LONG, host_out.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    send.await.unwrap().unwrap();
    assert_eq!(receive_id(&mut transport).await, 5);
}

#[tokio::test]
async fn sdk_error_response_releases_its_string_id_only_after_write() {
    let (mut transport, mut host_in, host_out, _) = setup(LONG);
    for id in [json!("one"), json!(2), json!(3), json!(4)] {
        write(&mut host_in, request(id.clone())).await;
        assert_eq!(receive_id(&mut transport).await, id);
    }
    let error: TxJsonRpcMessage<RoleServer> = serde_json::from_value(
        json!({"jsonrpc":"2.0", "id":"one", "error":{"code":-32603,"message":"e".repeat(1000)}}),
    )
    .unwrap();
    let send = tokio::spawn(transport.send(error));
    write(&mut host_in, request(5)).await;
    assert!(timeout(SHORT, transport.receive()).await.is_err());
    let mut line = String::new();
    timeout(LONG, BufReader::new(host_out).read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    send.await.unwrap().unwrap();
    assert_eq!(receive_id(&mut transport).await, 5);
}

#[tokio::test]
async fn duplicate_inflight_id_terminates_without_overwriting_budget() {
    let (mut transport, mut host_in, _host_out, _) = setup(LONG);
    write(&mut host_in, request("same")).await;
    assert_eq!(receive_id(&mut transport).await, "same");
    write(&mut host_in, request("same")).await;
    assert!(timeout(LONG, transport.receive()).await.unwrap().is_none());
    assert_eq!(
        transport.send(response("same")).await.unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}

#[tokio::test]
async fn writer_error_terminates_and_unblocks_pending_admission() {
    let (mut transport, mut host_in, host_out, _) = setup(LONG);
    fill(&mut transport, &mut host_in).await;
    write(&mut host_in, request(5)).await;
    assert!(timeout(SHORT, transport.receive()).await.is_err());
    drop(host_out);
    assert!(transport.send(response(1)).await.is_err());
    assert!(timeout(LONG, transport.receive()).await.unwrap().is_none());
    assert!(transport.send(response(2)).await.is_err());
}

#[tokio::test]
async fn partial_write_timeout_never_resumes_or_writes_the_next_json_message() {
    let (mut transport, mut host_in, mut host_out, _) = setup(Duration::from_millis(100));
    fill(&mut transport, &mut host_in).await;
    let first = tokio::spawn(transport.send(response(1)));
    let second = tokio::spawn(transport.send(response(2)));
    assert_eq!(
        first.await.unwrap().unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(second.await.unwrap().is_err());
    assert!(timeout(LONG, transport.receive()).await.unwrap().is_none());
    assert!(transport.send(response(3)).await.is_err());
    let mut bytes = Vec::new();
    timeout(LONG, host_out.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes.len(), 64, "only the first message prefix was written");
    assert!(!bytes.contains(&b'\n'));
}

#[tokio::test]
async fn dropped_send_future_is_terminal_even_with_other_sends_waiting() {
    let (mut transport, mut host_in, mut host_out, _) = setup(LONG);
    fill(&mut transport, &mut host_in).await;
    let first = tokio::spawn(transport.send(response(1)));
    let second = tokio::spawn(transport.send(response(2)));
    assert!(timeout(SHORT, transport.receive()).await.is_err());
    first.abort();
    assert!(first.await.is_err());
    assert!(second.await.unwrap().is_err());
    assert!(transport.receive().await.is_none());
    let mut bytes = Vec::new();
    host_out.read_to_end(&mut bytes).await.unwrap();
    assert!(bytes.len() <= 64);
    assert!(!bytes.contains(&b'\n'));
}

#[tokio::test]
async fn eof_at_full_capacity_stops_reads_but_preserves_sdk_response_drain() {
    let (mut transport, mut host_in, host_out, _) = setup(LONG);
    fill(&mut transport, &mut host_in).await;
    host_in.shutdown().await.unwrap();
    assert!(timeout(LONG, transport.receive()).await.unwrap().is_none());
    let send = tokio::spawn(transport.send(response(1)));
    let mut line = String::new();
    timeout(LONG, BufReader::new(host_out).read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    send.await.unwrap().unwrap();
    assert!(transport.receive().await.is_none());
    transport.close().await.unwrap();
    assert!(transport.send(response(2)).await.is_err());
}

#[tokio::test]
async fn sdk_internal_protocol_error_write_has_deadline_and_is_terminal() {
    let (input, mut host_in) = tokio::io::duplex(1024);
    let (output, mut host_out) = tokio::io::duplex(8);
    let mut transport = item_mcp::transport::from_io(input, output, Duration::from_millis(100));
    // This well-formed invalid message causes AsyncRwTransport::receive itself
    // to write an error, not Transport::send; the byte writer must enforce it.
    write(&mut host_in, json!({"invalid":"request"})).await;
    assert!(timeout(LONG, transport.receive()).await.unwrap().is_none());
    assert!(transport.send(response(1)).await.is_err());
    transport.close().await.unwrap();
    let mut bytes = Vec::new();
    timeout(LONG, host_out.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes.len(), 8);
    assert!(!bytes.contains(&b'\n'));
}

#[derive(Clone)]
struct CancelServer {
    started: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
}
impl ServerHandler for CancelServer {
    async fn call_tool(
        &self,
        _: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        self.started.fetch_add(1, Ordering::SeqCst);
        context.ct.cancelled().await;
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        Ok(CallToolResult::success(vec![]).into())
    }
}
async fn until(counter: &AtomicUsize, value: usize) {
    timeout(LONG, async {
        while counter.load(Ordering::SeqCst) < value {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn real_sdk_cancel_drops_handler_response_and_frees_capacity() {
    let (transport, mut host_in, host_out, _) = setup(LONG);
    let started = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let server = CancelServer {
        started: started.clone(),
        cancelled: cancelled.clone(),
    };
    let serve = tokio::spawn(async move { server.serve(transport).await.unwrap() });
    write(&mut host_in, json!({"jsonrpc":"2.0", "id":0, "method":"initialize", "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
    let mut reader = BufReader::new(host_out);
    let mut line = String::new();
    timeout(LONG, reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["protocolVersion"],
        "2024-11-05"
    );
    write(
        &mut host_in,
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
    )
    .await;
    let running = timeout(LONG, serve).await.unwrap().unwrap();
    for id in 1..=4 {
        write(
            &mut host_in,
            json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":"wait"}}),
        )
        .await;
    }
    until(&started, 4).await;
    write(&mut host_in, cancel(1)).await;
    until(&cancelled, 1).await;
    write(
        &mut host_in,
        json!({"jsonrpc":"2.0", "id":5, "method":"tools/call", "params":{"name":"wait"}}),
    )
    .await;
    until(&started, 5).await;
    write(&mut host_in, request(6)).await;
    assert!(
        timeout(SHORT, reader.read_line(&mut String::new()))
            .await
            .is_err()
    );
    // This cancellation is behind the retained lookahead and must not bypass it.
    // Cancel the service explicitly to exercise close without an unbounded drain.
    timeout(LONG, running.cancel()).await.unwrap().unwrap();
}
