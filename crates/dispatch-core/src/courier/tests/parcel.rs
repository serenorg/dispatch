use super::*;

#[test]
fn resolve_prompt_omits_eval_files() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nevals = [\"evals/smoke.eval\"]\n\n[agent.instructions]\nsoul = \"SOUL.md\"\nskill = \"SKILL.md\"\nmemory = \"MEMORY.md\"\n",
        &[
            ("SOUL.md", "Soul body"),
            ("SKILL.md", "Skill body"),
            ("MEMORY.md", "Memory body"),
            ("evals/smoke.eval", "assert output contains ok"),
        ],
    );

    let prompt = resolve_prompt_text(&test_parcel.parcel).unwrap();
    assert!(prompt.contains("# SOUL"));
    assert!(prompt.contains("# SKILL"));
    assert!(prompt.contains("# MEMORY"));
    assert!(!prompt.contains("smoke.eval"));
    assert!(!prompt.contains("# EVAL"));
    assert!(
        test_parcel
            .parcel
            .config
            .instructions
            .iter()
            .any(|instruction| instruction.kind == InstructionKind::Eval
                && instruction.packaged_path == "evals/smoke.eval")
    );
    assert!(
        test_parcel
            .parcel
            .parcel_dir
            .join("context/evals/smoke.eval")
            .is_file()
    );
}

#[test]
fn resolve_prompt_strips_agent_skill_frontmatter_for_skill_directories() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        &[
            (
                "file-analyst/SKILL.md",
                "---\nname: file-analyst\ndescription: Analyze files\nmetadata:\n  dispatch-manifest: skill.toml\n---\nUse the file tools before answering.\n",
            ),
            (
                "file-analyst/skill.toml",
                "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
            ),
            (
                "file-analyst/scripts/read_file.sh",
                "#!/bin/sh\ncat \"$1\"\n",
            ),
        ],
    );

    let prompt = resolve_prompt_text(&test_parcel.parcel).unwrap();
    assert!(prompt.contains("# SKILL"));
    assert!(prompt.contains("Use the file tools before answering."));
    assert!(!prompt.contains("dispatch-manifest"));
    assert!(!prompt.contains("name: file-analyst"));
    assert!(!prompt.contains("description: Analyze files"));
}

#[test]
fn resolve_prompt_keeps_file_based_skill_frontmatter_unchanged() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nskill = \"SKILL.md\"\n",
        &[(
            "SKILL.md",
            "---\nname: file-analyst\ndescription: Analyze files\n---\nUse the file tools before answering.\n",
        )],
    );

    let prompt = resolve_prompt_text(&test_parcel.parcel).unwrap();
    assert!(prompt.contains("name: file-analyst"));
    assert!(prompt.contains("description: Analyze files"));
    assert!(prompt.contains("Use the file tools before answering."));
}

#[test]
fn collect_skill_allowed_tools_returns_skill_annotations() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        &[
            (
                "file-analyst/SKILL.md",
                "---\nname: file-analyst\ndescription: Analyze files\nallowed-tools:\n  - Bash\n  - Read\n---\nUse the file tools before answering.\n",
            ),
            (
                "file-analyst/skill.toml",
                "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
            ),
            (
                "file-analyst/scripts/read_file.sh",
                "#!/bin/sh\ncat \"$1\"\n",
            ),
        ],
    );

    let allowed = collect_skill_allowed_tools(&test_parcel.parcel);
    assert_eq!(
        allowed.get("file-analyst"),
        Some(&vec!["Bash".to_string(), "Read".to_string()])
    );
}

#[test]
fn resolve_prompt_includes_extended_workspace_files() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nidentity = \"IDENTITY.md\"\nsoul = \"SOUL.md\"\nagents = \"AGENTS.md\"\nuser = \"USER.md\"\ntools = \"TOOLS.md\"\nmemory = \"MEMORY.md\"\n",
        &[
            ("IDENTITY.md", "Name: Demo"),
            ("SOUL.md", "Soul body"),
            ("AGENTS.md", "Workflow body"),
            ("USER.md", "User body"),
            ("TOOLS.md", "Tool body"),
            ("MEMORY.md", "Memory body"),
        ],
    );

    let prompt = resolve_prompt_text(&test_parcel.parcel).unwrap();
    assert!(prompt.contains("# IDENTITY"));
    assert!(prompt.contains("Name: Demo"));
    assert!(prompt.contains("# AGENTS"));
    assert!(prompt.contains("Workflow body"));
    assert!(prompt.contains("# USER"));
    assert!(prompt.contains("# TOOLS"));
}

#[test]
fn list_local_tools_uses_typed_manifest() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"job\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/demo.py\"\nalias = \"demo\"\nrunner = { command = \"python3\", args = [\"-u\"] }\n",
        &[("tools/demo.py", "print('ok')")],
    );

    let tools = list_local_tools(&test_parcel.parcel);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].alias, "demo");
    assert_eq!(tools[0].command(), "python3");
    assert_eq!(tools[0].args(), ["-u".to_string()]);
    assert_eq!(tools[0].transport(), LocalToolTransport::Local);
}

#[test]
fn list_local_tools_includes_a2a_tools() {
    let test_parcel = build_test_parcel(
        "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"job\"\n\n[[agent.secrets]]\nname = \"A2A_TOKEN\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\ndiscovery = \"card\"\nexpect_agent_name = \"remote-broker\"\nexpect_card_sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\ndescription = \"Delegate to broker\"\nschema = \"schemas/input.json\"\n\n[agent.tools.auth]\nscheme = \"bearer\"\nsecret_name = \"A2A_TOKEN\"\n",
        &[(
            "schemas/input.json",
            "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}\n",
        )],
    );

    let tools = list_local_tools(&test_parcel.parcel);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].alias, "broker");
    assert_eq!(tools[0].transport(), LocalToolTransport::A2a);
    assert_eq!(tools[0].endpoint_url(), Some("https://broker.example.com"));
    assert_eq!(tools[0].endpoint_mode(), Some(A2aEndpointMode::Card));
    assert_eq!(tools[0].auth_scheme(), Some(A2aAuthScheme::Bearer));
    assert_eq!(tools[0].auth_scheme(), Some(A2aAuthScheme::Bearer));
    assert_eq!(tools[0].auth_username_secret_name(), None);
    assert_eq!(tools[0].auth_password_secret_name(), None);
    assert_eq!(tools[0].auth_header_name(), None);
    assert_eq!(tools[0].expected_agent_name(), Some("remote-broker"));
    assert_eq!(
        tools[0].expected_card_sha256(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(tools[0].command(), "dispatch-a2a");
}
