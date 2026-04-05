use super::{
    CourierError, CourierSession, ToolRunResult,
    checkpoint_store::{checkpoint_delete, checkpoint_get, checkpoint_list, checkpoint_put},
    memory_store::{
        memory_delete, memory_delete_range, memory_get, memory_list, memory_list_range,
        memory_mount_path, memory_put, memory_put_many,
    },
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

fn parse_memory_ref(token: &str) -> (&str, &str) {
    match token.split_once(':') {
        Some((namespace, key)) if !namespace.is_empty() && !key.is_empty() => (namespace, key),
        _ => ("default", token),
    }
}

fn default_memory_namespace() -> String {
    "default".to_string()
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryGetInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    key: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryPutInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryListInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryRangeInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    start_key: Option<String>,
    end_key: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct BuiltinMemoryPutEntry {
    pub(super) key: String,
    pub(super) value: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryPutManyInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    entries: Vec<BuiltinMemoryPutEntry>,
}

#[derive(Debug, Deserialize)]
struct BuiltinCheckpointGetInput {
    name: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinCheckpointPutInput {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinCheckpointListInput {
    prefix: Option<String>,
}

fn parse_builtin_tool_input<T>(tool: &str, input: &str) -> Result<T, CourierError>
where
    T: DeserializeOwned,
{
    serde_json::from_str::<T>(input).map_err(|error| CourierError::InvalidBuiltinToolInput {
        tool: tool.to_string(),
        message: error.to_string(),
    })
}

pub(super) fn execute_builtin_tool(
    session: &CourierSession,
    capability: &str,
    input: &str,
) -> Result<ToolRunResult, CourierError> {
    let stdout = match capability {
        "memory_get" => {
            let input: BuiltinMemoryGetInput = parse_builtin_tool_input(capability, input)?;
            match memory_get(session, &input.namespace, &input.key)? {
                Some(entry) => format!("{}:{} = {}", entry.namespace, entry.key, entry.value),
                None => format!("No memory entry for {}:{}", input.namespace, input.key),
            }
        }
        "memory_put" => {
            let input: BuiltinMemoryPutInput = parse_builtin_tool_input(capability, input)?;
            let replaced = memory_put(session, &input.namespace, &input.key, &input.value)?;
            if replaced {
                format!("Updated memory {}:{}", input.namespace, input.key)
            } else {
                format!("Stored memory {}:{}", input.namespace, input.key)
            }
        }
        "memory_delete" => {
            let input: BuiltinMemoryGetInput = parse_builtin_tool_input(capability, input)?;
            let deleted = memory_delete(session, &input.namespace, &input.key)?;
            if deleted {
                format!("Deleted memory {}:{}", input.namespace, input.key)
            } else {
                format!("No memory entry for {}:{}", input.namespace, input.key)
            }
        }
        "memory_list" => {
            let input: BuiltinMemoryListInput = parse_builtin_tool_input(capability, input)?;
            let entries = memory_list(session, &input.namespace, input.prefix.as_deref())?;
            if entries.is_empty() {
                format!("No memory entries in namespace `{}`.", input.namespace)
            } else {
                entries
                    .into_iter()
                    .map(|entry| format!("{}:{} = {}", entry.namespace, entry.key, entry.value))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "memory_list_range" => {
            let input: BuiltinMemoryRangeInput = parse_builtin_tool_input(capability, input)?;
            if input.limit == Some(0) {
                return Err(CourierError::InvalidBuiltinToolInput {
                    tool: capability.to_string(),
                    message: "`limit` must be at least 1".to_string(),
                });
            }
            let entries = memory_list_range(
                session,
                &input.namespace,
                input.start_key.as_deref(),
                input.end_key.as_deref(),
                input.limit,
            )?;
            if entries.is_empty() {
                format!("No memory entries in namespace `{}`.", input.namespace)
            } else {
                entries
                    .into_iter()
                    .map(|entry| format!("{}:{} = {}", entry.namespace, entry.key, entry.value))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "memory_delete_range" => {
            let input: BuiltinMemoryRangeInput = parse_builtin_tool_input(capability, input)?;
            let deleted = memory_delete_range(
                session,
                &input.namespace,
                input.start_key.as_deref(),
                input.end_key.as_deref(),
            )?;
            if deleted == 0 {
                format!(
                    "No memory entries deleted from namespace `{}`.",
                    input.namespace
                )
            } else {
                format!(
                    "Deleted {} memory entr{} from namespace `{}`.",
                    deleted,
                    if deleted == 1 { "y" } else { "ies" },
                    input.namespace
                )
            }
        }
        "memory_put_many" => {
            let input: BuiltinMemoryPutManyInput = parse_builtin_tool_input(capability, input)?;
            let replaced = memory_put_many(session, &input.namespace, &input.entries)?;
            let stored = input.entries.len().saturating_sub(replaced);
            format!(
                "Stored {} and updated {} memory entr{} in namespace `{}`.",
                stored,
                replaced,
                if input.entries.len() == 1 { "y" } else { "ies" },
                input.namespace
            )
        }
        "checkpoint_get" => {
            let input: BuiltinCheckpointGetInput = parse_builtin_tool_input(capability, input)?;
            match checkpoint_get(session, &input.name)? {
                Some(entry) => format!("{} = {}", entry.name, entry.value),
                None => format!("No checkpoint named `{}`", input.name),
            }
        }
        "checkpoint_put" => {
            let input: BuiltinCheckpointPutInput = parse_builtin_tool_input(capability, input)?;
            let replaced = checkpoint_put(session, &input.name, &input.value)?;
            if replaced {
                format!("Updated checkpoint `{}`", input.name)
            } else {
                format!("Stored checkpoint `{}`", input.name)
            }
        }
        "checkpoint_delete" => {
            let input: BuiltinCheckpointGetInput = parse_builtin_tool_input(capability, input)?;
            let deleted = checkpoint_delete(session, &input.name)?;
            if deleted {
                format!("Deleted checkpoint `{}`", input.name)
            } else {
                format!("No checkpoint named `{}`", input.name)
            }
        }
        "checkpoint_list" => {
            let input: BuiltinCheckpointListInput = parse_builtin_tool_input(capability, input)?;
            let entries = checkpoint_list(session, input.prefix.as_deref())?;
            if entries.is_empty() {
                "No checkpoints stored.".to_string()
            } else {
                entries
                    .into_iter()
                    .map(|entry| format!("{} = {}", entry.name, entry.value))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        _ => {
            return Err(CourierError::InvalidBuiltinToolInput {
                tool: capability.to_string(),
                message: "unsupported builtin capability for native tool execution".to_string(),
            });
        }
    };

    Ok(ToolRunResult {
        tool: capability.to_string(),
        command: "dispatch-builtin".to_string(),
        args: vec![capability.to_string()],
        exit_code: 0,
        stdout,
        stderr: String::new(),
    })
}

pub(super) fn handle_native_memory_command(
    session: &CourierSession,
    command: &str,
) -> Result<String, CourierError> {
    let Some(rest) = command.strip_prefix("/memory") else {
        return Ok("Usage: /memory <put|get|delete|list> ...".to_string());
    };
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Ok("Usage: /memory <put|get|delete|list> ...".to_string());
    }
    if memory_mount_path(session).is_none() {
        return Ok("No sqlite memory mount is configured for this parcel.".to_string());
    }

    let mut parts = trimmed.splitn(3, ' ');
    let verb = parts.next().unwrap_or_default();
    match verb {
        "put" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            let value = parts.next().unwrap_or_default().trim();
            if key_ref.is_empty() || value.is_empty() {
                return Ok("Usage: /memory put <key|namespace:key> <value>".to_string());
            }
            let (namespace, key) = parse_memory_ref(key_ref);
            let replaced = memory_put(session, namespace, key, value)?;
            Ok(if replaced {
                format!("Updated memory {}:{}", namespace, key)
            } else {
                format!("Stored memory {}:{}", namespace, key)
            })
        }
        "get" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            if key_ref.is_empty() {
                return Ok("Usage: /memory get <key|namespace:key>".to_string());
            }
            let (namespace, key) = parse_memory_ref(key_ref);
            match memory_get(session, namespace, key)? {
                Some(entry) => Ok(format!(
                    "{}:{} = {}",
                    entry.namespace, entry.key, entry.value
                )),
                None => Ok(format!("No memory entry for {}:{}", namespace, key)),
            }
        }
        "delete" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            if key_ref.is_empty() {
                return Ok("Usage: /memory delete <key|namespace:key>".to_string());
            }
            let (namespace, key) = parse_memory_ref(key_ref);
            let deleted = memory_delete(session, namespace, key)?;
            Ok(if deleted {
                format!("Deleted memory {}:{}", namespace, key)
            } else {
                format!("No memory entry for {}:{}", namespace, key)
            })
        }
        "list" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            let (namespace, prefix) = if key_ref.is_empty() {
                ("default", None)
            } else {
                let (namespace, key) = parse_memory_ref(key_ref);
                (namespace, Some(key))
            };
            let entries = memory_list(session, namespace, prefix)?;
            if entries.is_empty() {
                return Ok(format!("No memory entries in namespace `{namespace}`."));
            }
            Ok(entries
                .into_iter()
                .map(|entry| format!("{}:{} = {}", entry.namespace, entry.key, entry.value))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        _ => Ok("Usage: /memory <put|get|delete|list> ...".to_string()),
    }
}
