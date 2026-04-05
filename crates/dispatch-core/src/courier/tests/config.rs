use super::*;

#[test]
fn configured_model_id_uses_env_when_primary_missing() {
    let model = configured_model_id_with(None, |name| match name {
        "LLM_MODEL" => Some("claude-sonnet-4".to_string()),
        _ => None,
    });

    assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
}

#[test]
fn configured_context_token_limit_uses_last_valid_context_limit() {
    let limits = vec![
        crate::manifest::LimitSpec {
            scope: "ITERATIONS".to_string(),
            value: "10".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "CONTEXT_TOKENS".to_string(),
            value: "16000".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "CONTEXT_TOKENS".to_string(),
            value: "32000".to_string(),
            qualifiers: Vec::new(),
        },
    ];

    assert_eq!(configured_context_token_limit(&limits), Some(32000));
}

#[test]
fn configured_llm_timeout_ms_uses_last_matching_timeout() {
    let timeouts = vec![
        crate::manifest::TimeoutSpec {
            scope: "LLM".to_string(),
            duration: "15s".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::TimeoutSpec {
            scope: "TOOL".to_string(),
            duration: "50ms".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::TimeoutSpec {
            scope: "LLM".to_string(),
            duration: "1200ms".to_string(),
            qualifiers: Vec::new(),
        },
    ];

    assert_eq!(configured_llm_timeout_ms(&timeouts).unwrap(), Some(1200));
}

#[test]
fn configured_tool_limits_use_last_valid_values() {
    let limits = vec![
        crate::manifest::LimitSpec {
            scope: "TOOL_CALLS".to_string(),
            value: "2".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_OUTPUT".to_string(),
            value: "0".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_CALLS".to_string(),
            value: "5".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_OUTPUT".to_string(),
            value: "1024".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_ROUNDS".to_string(),
            value: "0".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_ROUNDS".to_string(),
            value: "6".to_string(),
            qualifiers: Vec::new(),
        },
    ];

    assert_eq!(configured_tool_call_limit(&limits), Some(5));
    assert_eq!(configured_tool_output_limit(&limits), Some(1024));
    assert_eq!(configured_tool_round_limit(&limits), Some(6));
}

#[test]
fn truncate_tool_output_preserves_utf8_boundaries() {
    let output = "hello π world and a much longer tool output payload".to_string();
    let truncated = truncate_tool_output(output, Some(40));
    assert!(truncated.is_char_boundary(truncated.len()));
    assert!(truncated.contains("[dispatch truncated tool output]"));
}

#[test]
fn courier_error_retryability_is_classified() {
    assert!(CourierError::ModelBackendRequest("network".to_string()).is_retryable());
    assert!(
        !CourierError::ToolCallLimitExceeded {
            limit: 2,
            attempted: 3
        }
        .is_retryable()
    );
    assert!(
        !CourierError::ToolTimedOut {
            tool: "slow".to_string(),
            timeout: "TOOL".to_string()
        }
        .is_retryable()
    );
    assert!(
        !CourierError::RunTimedOut {
            session_id: "session-1".to_string(),
            timeout: "RUN".to_string()
        }
        .is_retryable()
    );
    assert!(
        !CourierError::MissingSecret {
            name: "OPENAI_API_KEY".to_string()
        }
        .is_retryable()
    );
}

#[test]
fn normalize_local_tool_input_extracts_function_style_text_payload() {
    let tool = LocalToolSpec {
        alias: "demo".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::Local {
            packaged_path: "tools/demo.sh".to_string(),
            command: "bash".to_string(),
            args: Vec::new(),
        },
    };

    let normalized = normalize_local_tool_input(&tool, "{\"input\":\"echo hi\"}").unwrap();
    assert_eq!(normalized.as_ref(), "echo hi");
}
