mod support;

use serde_json::{Value, json};
use support::Client;

#[test]
fn stdio_handshake_lists_and_calls_typed_read_only_tool() {
    let mut client = Client::new(true);
    client.initialize("2025-11-25");
    let response = client.request(2, "tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 7);
    let frame_tool = tools
        .iter()
        .find(|tool| tool["name"] == "get_camera_frame")
        .unwrap();
    assert_eq!(frame_tool["annotations"]["readOnlyHint"], false);
    assert_eq!(frame_tool["annotations"]["destructiveHint"], false);
    assert_eq!(frame_tool["annotations"]["idempotentHint"], false);
    assert_eq!(frame_tool["inputSchema"]["additionalProperties"], false);
    assert_eq!(frame_tool["inputSchema"]["required"], json!(["camera_id"]));
    assert_eq!(frame_tool["outputSchema"]["type"], "object");
    assert_eq!(
        frame_tool["outputSchema"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for name in [
        "server_info",
        "list_recorded_items",
        "list_observations",
        "locate_item",
        "get_observation",
        "get_item_images",
    ] {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["outputSchema"]["type"], "object");
    }
    let image_tool = tools
        .iter()
        .find(|tool| tool["name"] == "get_item_images")
        .unwrap();
    assert_eq!(
        image_tool["outputSchema"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        image_tool["outputSchema"]["oneOf"][1]["required"],
        json!(["error"])
    );
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "server_info")
        .unwrap();
    assert_eq!(tool["inputSchema"]["type"], "object");
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    assert_eq!(tool["outputSchema"]["type"], "object");
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert_eq!(tool["annotations"]["destructiveHint"], false);

    let response = client.request(
        3,
        "tools/call",
        json!({"name":"server_info", "arguments":{}}),
    );
    assert!(response.get("error").is_none(), "{response}");
    let result = &response["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(
        result["structuredContent"],
        json!({
            "name":"item-mcp", "version":env!("CARGO_PKG_VERSION"),
            "transport":"stdio", "mode":"queries_and_camera_frames"
        })
    );
    assert_eq!(result["content"][0]["type"], "text");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);

    let response = client.request(4, "ping", json!({}));
    assert!(response["result"].is_object(), "{response}");
    client.finish();
}

#[test]
fn default_stdio_supports_older_host_protocol() {
    let mut client = Client::new(false);
    client.initialize("2024-11-05");
    let response = client.request(2, "tools/list", json!({}));
    assert!(
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "server_info")
    );
    let response = client.request(3, "tools/call", json!({"name":"server_info"}));
    assert_eq!(
        response["result"]["structuredContent"]["transport"],
        "stdio"
    );
    client.finish();
}

#[test]
fn invalid_tool_calls_do_not_break_the_connection() {
    let mut client = Client::new(true);
    client.initialize("2025-03-26");
    let response = client.request(
        2,
        "tools/call",
        json!({"name":"unknown_tool", "arguments":{}}),
    );
    assert!(response["error"]["code"].is_number(), "{response}");
    let response = client.request(
        3,
        "tools/call",
        json!({"name":"server_info", "arguments":{"unexpected":true}}),
    );
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown field"),
        "{response}"
    );
    let response = client.request(
        4,
        "tools/call",
        json!({"name":"server_info", "arguments":{}}),
    );
    assert_eq!(
        response["result"]["structuredContent"]["mode"],
        "queries_and_camera_frames"
    );
    client.finish();
}
