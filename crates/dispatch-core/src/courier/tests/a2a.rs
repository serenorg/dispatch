use super::*;
use crate::courier::a2a::a2a_origin;

#[test]
fn native_courier_executes_a2a_tools_via_host_transport() {
    let server = start_test_a2a_server();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DESCRIPTION \"Delegate to broker\"
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool(&test_parcel.parcel, "broker", Some("hello remote")).unwrap();
    assert_eq!(result.tool, "broker");
    assert_eq!(result.command, "dispatch-a2a");
    assert!(result.stdout.contains("echo:hello remote"));
}

#[test]
fn native_courier_executes_a2a_tools_with_json_payloads() {
    let server = start_test_a2a_server();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} SCHEMA schemas/input.json
ENTRYPOINT job
",
            server.base_url
        ),
        &[(
            "schemas/input.json",
            "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}\n",
        )],
    );

    let result = run_local_tool(
        &test_parcel.parcel,
        "broker",
        Some("{\"query\":\"weather\"}"),
    )
    .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(
        output.pointer("/query").and_then(serde_json::Value::as_str),
        Some("weather")
    );
}

#[test]
fn native_courier_rejects_non_loopback_cleartext_a2a_urls() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL A2A broker URL http://example.com DISCOVERY direct
ENTRYPOINT job
",
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::A2aToolRequest { ref message, .. }
            if message.contains("must use https unless it targets a loopback host")
    ));
}

#[test]
fn native_courier_rejects_a2a_urls_with_embedded_credentials() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL A2A broker URL http://user:pass@127.0.0.1:7777 DISCOVERY direct
ENTRYPOINT job
",
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::A2aToolRequest { ref message, .. }
            if message.contains("must not embed credentials")
    ));
}

#[test]
fn native_courier_executes_a2a_tools_with_bearer_auth() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("Bearer topsecret".to_string()),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
SECRET A2A_TOKEN
TOOL A2A broker URL {} AUTH bearer A2A_TOKEN
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "A2A_TOKEN").then(|| "topsecret".to_string())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_executes_a2a_tools_with_header_auth() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("X-Api-Key: topsecret".to_string()),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
SECRET API_KEY
TOOL A2A broker URL {} AUTH header X-Api-Key API_KEY
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "API_KEY").then(|| "topsecret".to_string())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_executes_a2a_tools_with_basic_auth() {
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode("demo-user:topsecret")
    };
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some(format!("Basic {encoded}")),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
SECRET A2A_USER
SECRET A2A_PASSWORD
TOOL A2A broker URL {} AUTH basic A2A_USER A2A_PASSWORD
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result =
        run_local_tool_with_env(
            &test_parcel.parcel,
            "broker",
            Some("hello"),
            |name| match name {
                "A2A_USER" => Some("demo-user".to_string()),
                "A2A_PASSWORD" => Some("topsecret".to_string()),
                _ => None,
            },
        )
        .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_a2a_call_when_auth_secret_is_missing() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("Bearer topsecret".to_string()),
        ..Default::default()
    });
    let tool = LocalToolSpec {
        alias: "broker".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::A2a {
            endpoint_url: server.base_url.clone(),
            endpoint_mode: None,
            auth: Some(crate::manifest::A2aAuthConfig::Bearer {
                secret_name: "A2A_TOKEN".to_string(),
            }),
            expected_agent_name: None,
            expected_card_sha256: None,
        },
    };

    let error = execute_a2a_tool_with_env(&tool, Some("hello"), |_| None, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configured A2A bearer auth secret is not available")
    );
}

#[test]
fn native_courier_rejects_a2a_header_auth_when_secret_is_missing() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("X-Api-Key: topsecret".to_string()),
        ..Default::default()
    });
    let tool = LocalToolSpec {
        alias: "broker".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::A2a {
            endpoint_url: server.base_url.clone(),
            endpoint_mode: None,
            auth: Some(crate::manifest::A2aAuthConfig::Header {
                header_name: "X-Api-Key".to_string(),
                secret_name: "API_KEY".to_string(),
            }),
            expected_agent_name: None,
            expected_card_sha256: None,
        },
    };

    let error = execute_a2a_tool_with_env(&tool, Some("hello"), |_| None, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configured A2A header auth secret is not available")
    );
}

#[test]
fn native_courier_rejects_a2a_basic_auth_when_username_secret_is_missing() {
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode("demo-user:topsecret")
    };
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some(format!("Basic {encoded}")),
        ..Default::default()
    });
    let tool = LocalToolSpec {
        alias: "broker".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::A2a {
            endpoint_url: server.base_url.clone(),
            endpoint_mode: None,
            auth: Some(crate::manifest::A2aAuthConfig::Basic {
                username_secret_name: "A2A_USER".to_string(),
                password_secret_name: "A2A_PASSWORD".to_string(),
            }),
            expected_agent_name: None,
            expected_card_sha256: None,
        },
    };

    let error = execute_a2a_tool_with_env(
        &tool,
        Some("hello"),
        |name| (name == "A2A_PASSWORD").then(|| "topsecret".to_string()),
        None,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configured A2A basic auth username secret is not available")
    );
}

#[test]
fn native_courier_rejects_a2a_basic_auth_when_password_secret_is_missing() {
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode("demo-user:topsecret")
    };
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some(format!("Basic {encoded}")),
        ..Default::default()
    });
    let tool = LocalToolSpec {
        alias: "broker".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::A2a {
            endpoint_url: server.base_url.clone(),
            endpoint_mode: None,
            auth: Some(crate::manifest::A2aAuthConfig::Basic {
                username_secret_name: "A2A_USER".to_string(),
                password_secret_name: "A2A_PASSWORD".to_string(),
            }),
            expected_agent_name: None,
            expected_card_sha256: None,
        },
    };

    let error = execute_a2a_tool_with_env(
        &tool,
        Some("hello"),
        |name| (name == "A2A_USER").then(|| "demo-user".to_string()),
        None,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configured A2A basic auth password secret is not available")
    );
}

#[test]
fn native_courier_rejects_a2a_agent_name_mismatch() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: Some("actual-agent".to_string()),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_AGENT_NAME expected-agent
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent card name mismatch: expected `expected-agent`, got `actual-agent`")
    );
}

#[test]
fn native_courier_rejects_a2a_agent_name_requirement_when_card_has_no_name() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: None,
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_AGENT_NAME expected-agent
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent card did not include `name`, but `expected-agent` was required")
    );
}

#[test]
fn native_courier_rejects_a2a_card_digest_mismatch() {
    let server = start_test_a2a_server();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_CARD_SHA256 ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(error
        .to_string()
        .contains("agent card digest mismatch: expected `ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff`"));
}

#[test]
fn native_courier_accepts_matching_a2a_card_digest() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: Some("demo-a2a".to_string()),
        ..Default::default()
    });
    let expected_card_sha256 = encode_hex(Sha256::digest(
        serde_json::to_vec(&serde_json::json!({
            "name": "demo-a2a",
            "url": format!("{}/a2a", server.base_url)
        }))
        .unwrap(),
    ));
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_CARD_SHA256 {}
ENTRYPOINT job
",
            server.base_url, expected_card_sha256
        ),
        &[],
    );

    let result = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_a2a_card_origin_pivot() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        card_url: Some("https://evil.example.com/a2a".to_string()),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("discovered agent card URL must stay on the declared origin")
    );
}

#[test]
fn native_courier_enforces_tool_timeout_for_a2a_tools() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        response_delay: Duration::from_secs(5),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TIMEOUT TOOL 200ms
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "broker" && timeout == "TOOL"
    ));
}

#[test]
fn native_courier_requires_card_discovery_when_configured() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        publish_card: false,
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DISCOVERY card
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent card discovery failed for required `DISCOVERY card` mode")
    );
}

#[test]
fn native_courier_polls_non_completed_a2a_tasks_until_completion() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        task_state: "working".to_string(),
        task_status_message: "queued for async execution".to_string(),
        task_get_state: Some("completed".to_string()),
        task_get_status_message: Some("done".to_string()),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_times_out_polling_non_completed_a2a_tasks() {
    let cancel_count = Arc::new(AtomicU64::new(0));
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        task_state: "working".to_string(),
        task_status_message: "queued for async execution".to_string(),
        task_get_state: Some("working".to_string()),
        task_get_status_message: Some("still running".to_string()),
        cancel_count: Some(cancel_count.clone()),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TIMEOUT TOOL 200ms
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "broker" && timeout == "TOOL"
    ));
    assert_eq!(cancel_count.load(Ordering::Relaxed), 1);
}

#[test]
fn native_courier_surfaces_a2a_json_rpc_errors() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        rpc_error: Some((-32001, "remote agent unavailable".to_string())),
        ..Default::default()
    });
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_parcel.parcel, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("JSON-RPC error -32001: remote agent unavailable")
    );
}

#[test]
fn native_courier_rejects_a2a_url_outside_operator_allowlist() {
    let server = start_test_a2a_server();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_ALLOWED_ORIGINS")
            .then(|| "https://agents.example.com,broker.internal".to_string())
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not allowed by DISPATCH_A2A_ALLOWED_ORIGINS")
    );
}

#[test]
fn native_courier_allows_a2a_url_with_matching_operator_allowlist_origin() {
    let server = start_test_a2a_server();
    let parsed = url::Url::parse(&server.base_url).unwrap();
    let origin = a2a_origin(&parsed).unwrap();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_ALLOWED_ORIGINS").then(|| origin.clone())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_a2a_url_when_operator_allowlist_is_explicitly_empty() {
    let server = start_test_a2a_server();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_ALLOWED_ORIGINS").then(String::new)
    })
    .unwrap_err();
    assert!(error.to_string().contains("resolved to an empty allowlist"));
}

#[test]
fn native_courier_rejects_a2a_url_outside_operator_trust_policy() {
    let server = start_test_a2a_server();
    let dir = tempdir().unwrap();
    let policy_path = dir.path().join("a2a-trust.toml");
    fs::write(
        &policy_path,
        "[[rules]]\norigin_prefix = \"https://agents.example.com\"\n",
    )
    .unwrap();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_TRUST_POLICY").then(|| policy_path.display().to_string())
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not allowed by DISPATCH_A2A_TRUST_POLICY")
    );
}

#[test]
fn native_courier_enforces_operator_a2a_trust_policy_identity() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: Some("planner-agent".to_string()),
        ..Default::default()
    });
    let dir = tempdir().unwrap();
    let policy_path = dir.path().join("a2a-trust.toml");
    let card_body = serde_json::to_vec(&serde_json::json!({
        "name": "planner-agent",
        "url": format!("{}/a2a", server.base_url),
    }))
    .unwrap();
    let card_sha = encode_hex(Sha256::digest(card_body));
    fs::write(
        &policy_path,
        format!(
            "[[rules]]\nhostname = \"127.0.0.1\"\nexpected_agent_name = \"planner-agent\"\nexpected_card_sha256 = \"{}\"\n",
            card_sha
        ),
    )
    .unwrap();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_TRUST_POLICY").then(|| policy_path.display().to_string())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_direct_a2a_with_operator_identity_requirement() {
    let server = start_test_a2a_server();
    let dir = tempdir().unwrap();
    let policy_path = dir.path().join("a2a-trust.toml");
    fs::write(
        &policy_path,
        "[[rules]]\nhostname = \"127.0.0.1\"\nexpected_agent_name = \"planner-agent\"\n",
    )
    .unwrap();
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DISCOVERY direct
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_parcel.parcel, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_TRUST_POLICY").then(|| policy_path.display().to_string())
    })
    .unwrap_err();
    assert!(error.to_string().contains("DISCOVERY direct"));
}

#[test]
fn a2a_operator_policy_overrides_supply_allowed_origins_to_process_lookup() {
    let result = with_a2a_operator_policy_overrides(
        A2aOperatorPolicyOverrides {
            allowed_origins: Some("https://planner.example.com".to_string()),
            trust_policy: None,
        },
        || process_env_lookup("DISPATCH_A2A_ALLOWED_ORIGINS"),
    );
    assert_eq!(result.as_deref(), Some("https://planner.example.com"));
    assert!(a2a_operator_policy_override_value("DISPATCH_A2A_ALLOWED_ORIGINS").is_none());
}

#[test]
fn a2a_operator_policy_overrides_supply_trust_policy_to_process_lookup() {
    let result = with_a2a_operator_policy_overrides(
        A2aOperatorPolicyOverrides {
            allowed_origins: None,
            trust_policy: Some("/tmp/dispatch-a2a-policy.toml".to_string()),
        },
        || process_env_lookup("DISPATCH_A2A_TRUST_POLICY"),
    );
    assert_eq!(result.as_deref(), Some("/tmp/dispatch-a2a-policy.toml"));
    assert!(a2a_operator_policy_override_value("DISPATCH_A2A_TRUST_POLICY").is_none());
}
