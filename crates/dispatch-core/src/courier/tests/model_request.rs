use super::*;

#[test]
fn build_model_request_uses_primary_model_prompt_and_history() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
MODEL gpt-5-mini
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
    );

    let local_tools = list_local_tools(&test_image.image);
    let request = build_model_request(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        &local_tools,
    )
    .unwrap()
    .expect("expected model request");

    assert_eq!(request.model, "gpt-5-mini");
    assert!(request.instructions.contains("Soul body"));
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].content, "hello");
    assert!(request.tool_outputs.is_empty());
    assert!(request.previous_response_id.is_none());
}

#[test]
fn build_model_request_carries_model_persist_history_setting() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5.4 PROVIDER codex PERSIST_HISTORY false
ENTRYPOINT chat
",
        &[],
    );

    let request = build_model_request(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        &[],
    )
    .unwrap()
    .expect("expected model request");

    assert_eq!(request.provider.as_deref(), Some("codex"));
    assert_eq!(request.persist_history, Some(false));
}

#[test]
fn build_model_requests_only_passes_backend_state_to_codex_provider() {
    // When a parcel has a Codex primary and a non-Codex fallback, backend_state
    // (a Codex thread-state blob) must NOT be forwarded to the non-Codex fallback
    // request: other backends treat previous_response_id as their own opaque
    // continuation token and would send the Codex JSON blob to a provider API,
    // causing an API error.
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5.4 PROVIDER codex
FALLBACK gpt-5.4-mini PROVIDER openai
ENTRYPOINT chat
",
        &[],
    );

    let codex_thread_state = r#"{"thread_id":"thr_abc","rollout_path":null}"#;
    let requests = build_model_requests(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "follow up".to_string(),
        }],
        &[],
        None,
        Some(codex_thread_state),
    )
    .unwrap();

    assert_eq!(requests.len(), 2);
    // Codex primary gets the thread state.
    assert_eq!(
        requests[0].previous_response_id.as_deref(),
        Some(codex_thread_state),
    );
    // OpenAI fallback must not receive the Codex blob.
    assert_eq!(requests[1].provider.as_deref(), Some("openai"));
    assert!(requests[1].previous_response_id.is_none());
}

#[test]
fn build_model_request_uses_declared_tool_description() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo DESCRIPTION \"Look up a record by id. Input: JSON with an id field.\"
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let local_tools = list_local_tools(&test_image.image);
    let request = build_model_request(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        &local_tools,
    )
    .unwrap()
    .expect("expected model request");

    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "demo");
    assert_eq!(
        request.tools[0].description,
        "Look up a record by id. Input: JSON with an id field."
    );
    assert!(matches!(request.tools[0].format, ModelToolFormat::Text));
}

#[test]
fn build_model_request_loads_declared_tool_input_schema() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
        &[
            ("tools/demo.sh", "printf ok"),
            (
                "schemas/demo.json",
                "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  },\n  \"required\": [\"id\"]\n}",
            ),
        ],
    );

    let local_tools = list_local_tools(&test_image.image);
    let request = build_model_request(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        &local_tools,
    )
    .unwrap()
    .expect("expected model request");

    assert_eq!(request.tools.len(), 1);
    match &request.tools[0].format {
        ModelToolFormat::JsonSchema { schema } => {
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["required"][0], "id");
        }
        other => panic!("expected json schema tool format, got {other:?}"),
    }
}

#[test]
fn list_native_builtin_tools_only_exposes_supported_memory_capabilities() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get DESCRIPTION \"Read remembered state.\"
TOOL BUILTIN memory_list_range
TOOL BUILTIN memory_delete_range
TOOL BUILTIN memory_put_many
TOOL BUILTIN checkpoint_put
TOOL BUILTIN checkpoint_list
TOOL BUILTIN web_search
ENTRYPOINT chat
",
        &[],
    );

    let tools = list_native_builtin_tools(&test_image.image);
    assert_eq!(tools.len(), 7);
    assert_eq!(tools[0].capability, "memory_put");
    assert_eq!(tools[1].capability, "memory_get");
    assert_eq!(tools[2].capability, "memory_list_range");
    assert_eq!(tools[3].capability, "memory_delete_range");
    assert_eq!(tools[4].capability, "memory_put_many");
    assert_eq!(tools[5].capability, "checkpoint_put");
    assert_eq!(tools[6].capability, "checkpoint_list");
    assert_eq!(
        tools[1].description.as_deref(),
        Some("Read remembered state.")
    );
}

#[test]
fn build_model_request_includes_supported_builtin_memory_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
TOOL BUILTIN memory_list_range
TOOL BUILTIN memory_delete_range
TOOL BUILTIN memory_put_many
TOOL BUILTIN checkpoint_put
TOOL BUILTIN checkpoint_list
ENTRYPOINT chat
",
        &[],
    );

    let request = build_model_request(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "remember this".to_string(),
        }],
        &[],
    )
    .unwrap()
    .expect("expected model request");

    assert_eq!(request.tools.len(), 7);
    assert_eq!(request.tools[0].name, "memory_put");
    assert!(matches!(
        request.tools[0].format,
        ModelToolFormat::JsonSchema { .. }
    ));
    assert_eq!(request.tools[1].name, "memory_get");
    assert_eq!(request.tools[2].name, "memory_list_range");
    assert_eq!(request.tools[3].name, "memory_delete_range");
    assert_eq!(request.tools[4].name, "memory_put_many");
    assert_eq!(request.tools[5].name, "checkpoint_put");
    assert_eq!(request.tools[6].name, "checkpoint_list");
}

#[test]
fn build_model_request_rejects_tampered_packaged_tool_schema() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
        &[
            ("tools/demo.sh", "printf ok"),
            (
                "schemas/demo.json",
                "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  }\n}",
            ),
        ],
    );
    fs::write(
        test_image
            .image
            .parcel_dir
            .join("context/schemas/demo.json"),
        "{ \"type\": \"array\" }",
    )
    .unwrap();

    let local_tools = list_local_tools(&test_image.image);
    let error = build_model_request(&test_image.image, &[], &local_tools).unwrap_err();
    assert!(matches!(
        error,
        CourierError::ToolSchemaDigestMismatch { tool, .. } if tool == "demo"
    ));
}
