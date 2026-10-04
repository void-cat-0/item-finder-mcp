#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Value>,
    stdout: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<String>>,
}

impl Client {
    pub(crate) fn new(explicit_stdio: bool) -> Self {
        Self::start(explicit_stdio, None, None)
    }

    pub(crate) fn with_db(db: &std::path::Path) -> Self {
        Self::start(true, Some(db), None)
    }

    pub(crate) fn start(
        explicit_stdio: bool,
        db: Option<&std::path::Path>,
        env_db: Option<&std::path::Path>,
    ) -> Self {
        Self::start_with_daemon(explicit_stdio, db, env_db, None)
    }

    pub(crate) fn start_with_daemon(
        explicit_stdio: bool,
        db: Option<&std::path::Path>,
        env_db: Option<&std::path::Path>,
        daemon: Option<(&str, &str)>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_item-mcp"));
        command.env_remove("ITEM_DB");
        command.env_remove("ITEM_DAEMON_ENDPOINT");
        command.env_remove("ITEM_DAEMON_TOKEN");
        if let Some((endpoint, token)) = daemon {
            command.arg("--daemon-endpoint").arg(endpoint);
            command.env("ITEM_DAEMON_TOKEN", token);
        }
        if explicit_stdio {
            command.arg("--stdio");
        }
        if let Some(db) = db {
            command.arg("--db").arg(db);
        }
        if let Some(db) = env_db {
            command.env("ITEM_DB", db);
        }
        let mut child = command
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start item-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (sender, messages) = mpsc::channel();
        let stdout = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("read stdout");
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("non-JSON stdout: {line:?}: {error}"));
                assert_eq!(message["jsonrpc"], "2.0");
                sender.send(message).expect("deliver protocol message");
            }
        });
        let stderr = thread::spawn(move || {
            let mut text = String::new();
            stderr.read_to_string(&mut text).unwrap();
            text
        });
        Self {
            child,
            stdin: Some(stdin),
            messages,
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    pub(crate) fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
        let response = self.messages.recv_timeout(TIMEOUT).expect("MCP response");
        assert_eq!(response["id"], id, "unexpected response: {response}");
        response
    }

    pub(crate) fn tool(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        let response = self.request(id, "tools/call", json!({"name":name,"arguments":arguments}));
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["isError"], false, "{response}");
        let result = &response["result"];
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
        result["structuredContent"].clone()
    }

    pub(crate) fn tool_error(&mut self, id: i64, name: &str, arguments: Value, code: &str) {
        let response = self.request(id, "tools/call", json!({"name":name,"arguments":arguments}));
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"], code,
            "{response}"
        );
        let text = response.to_string();
        assert!(
            !text.contains("SELECT") && !text.contains("C:/Users"),
            "leaked internals: {text}"
        );
    }

    pub(crate) fn initialize(&mut self, version: &str) {
        let response = self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name":"item-mcp-stdio-test", "version":"1.0"}
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["protocolVersion"], version);
        assert_eq!(response["result"]["serverInfo"]["name"], "item-mcp");
        assert_eq!(
            response["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(response["result"]["capabilities"]["tools"].is_object());
        assert!(
            response["result"]["capabilities"]
                .get("resources")
                .is_none()
        );
        self.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    }

    pub(crate) fn finish(mut self) {
        self.stdin.take();
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "server did not exit after EOF");
            thread::sleep(Duration::from_millis(10));
        };
        self.stdout.take().unwrap().join().unwrap();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        assert!(status.success(), "server exited {status}: {stderr}");
        assert!(stderr.contains("starting item-mcp"), "{stderr}");
        assert!(stderr.contains("stdio transport closed"), "{stderr}");
        assert!(self.messages.try_recv().is_err(), "unexpected extra stdout");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
