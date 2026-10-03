//! The rmcp-backed session against the in-process fake.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use henk_mcp::McpSession as _;
use henk_mcp::testing::{FakeServer, echo_behaviour};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

#[tokio::test]
async fn lists_tools_and_calls_them() {
    let fake = FakeServer::new(
        vec![
            FakeServer::tool("get_pull_request", "Reads a PR.", &["owner", "repo"]),
            FakeServer::tool("list_issues", "Lists issues.", &["owner"]),
        ],
        echo_behaviour(),
    );
    let session = fake.connect("github").await;
    assert_eq!(session.alias(), "github");

    let tools = session.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "get_pull_request");
    assert_eq!(tools[0].description, "Reads a PR.");
    assert_eq!(
        tools[0].input_schema["properties"]["owner"]["type"],
        "string"
    );

    let outcome = session
        .call_tool("get_pull_request", json!({"owner": "o", "repo": "r"}))
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(
        outcome.text,
        "get_pull_request({\"owner\":\"o\",\"repo\":\"r\"})"
    );
    assert_eq!(fake.calls().len(), 1);
    assert_eq!(
        fake.calls()[0].arguments,
        json!({"owner": "o", "repo": "r"})
    );
    session.close().await;
}

#[tokio::test]
async fn error_results_are_flagged_not_raised() {
    let fake = FakeServer::new(vec![FakeServer::tool("boom", "Fails.", &[])], |_, _| {
        CallToolResult::error(vec![ContentBlock::text("it broke")])
    });
    let session = fake.connect("x").await;
    let outcome = session.call_tool("boom", json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert_eq!(outcome.text, "it broke");

    let unknown = session.call_tool("nope", json!({})).await.unwrap();
    assert!(unknown.is_error);
    assert!(unknown.text.contains("unknown tool"));
}

#[tokio::test]
async fn non_object_arguments_are_rejected_locally() {
    let fake = FakeServer::new(vec![FakeServer::tool("t", "", &[])], echo_behaviour());
    let session = fake.connect("x").await;
    let error = session.call_tool("t", json!([1, 2])).await.unwrap_err();
    assert!(matches!(error, henk_mcp::McpError::InvalidConfig { .. }));
    assert!(fake.calls().is_empty(), "nothing reached the server");
}
