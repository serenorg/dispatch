use super::{
    BufReader, Child, ChildStdout, CourierError, CourierEvent, CourierSession, LoadedParcel,
    PersistentPluginProcess,
};
use crate::plugin_protocol::{
    PluginRequest, PluginRequestEnvelope, PluginRequestId, PluginResponse, parse_jsonrpc_message,
    request_to_jsonrpc,
};
use std::{io::Write as _, sync::mpsc, time::Duration};

pub(super) type ParsedPluginResponse = (Option<PluginRequestId>, PluginResponse);

pub(super) fn write_plugin_request(
    child: &mut Child,
    courier_name: &str,
    protocol_version: u32,
    request: PluginRequest,
    close_stdin: bool,
) -> Result<PluginRequestId, CourierError> {
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: "plugin stdin was not captured".to_string(),
        })?;
    let request_id = PluginRequestId::Integer(1);
    write_plugin_request_to(
        stdin,
        courier_name,
        request_id.clone(),
        protocol_version,
        request,
    )?;
    if close_stdin {
        let _ = child.stdin.take();
    }
    Ok(request_id)
}

pub(super) fn write_plugin_request_to<W: std::io::Write>(
    mut writer: W,
    courier_name: &str,
    request_id: PluginRequestId,
    protocol_version: u32,
    request: PluginRequest,
) -> Result<(), CourierError> {
    let rpc_request = request_to_jsonrpc(
        request_id,
        &PluginRequestEnvelope {
            protocol_version,
            request,
        },
    )
    .map_err(|message| CourierError::PluginProtocol {
        courier: courier_name.to_string(),
        message,
    })?;
    serde_json::to_writer(&mut writer, &rpc_request).map_err(|source| {
        CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: format!("failed to serialize plugin request: {source}"),
        }
    })?;
    writer
        .write_all(b"\n")
        .map_err(|source| CourierError::WritePluginRequest {
            courier: courier_name.to_string(),
            source,
        })?;
    writer
        .flush()
        .map_err(|source| CourierError::WritePluginRequest {
            courier: courier_name.to_string(),
            source,
        })?;
    Ok(())
}

impl PersistentPluginProcess {
    pub(super) fn write_request(
        &mut self,
        protocol_version: u32,
        courier_name: &str,
        request: PluginRequest,
    ) -> Result<PluginRequestId, CourierError> {
        let request_id = PluginRequestId::Integer(1);
        write_plugin_request_to(
            &mut self.stdin,
            courier_name,
            request_id.clone(),
            protocol_version,
            request,
        )?;
        Ok(request_id)
    }

    pub(super) fn read_response(
        &mut self,
        courier_name: &str,
        expected_id: &PluginRequestId,
    ) -> Result<PluginResponse, CourierError> {
        match self.read_response_timeout(courier_name, expected_id, None)? {
            Some(response) => Ok(response),
            None => Err(CourierError::PluginProtocol {
                courier: courier_name.to_string(),
                message: "plugin response timed out".to_string(),
            }),
        }
    }

    pub(super) fn read_response_timeout(
        &mut self,
        courier_name: &str,
        expected_id: &PluginRequestId,
        timeout: Option<Duration>,
    ) -> Result<Option<PluginResponse>, CourierError> {
        let received = match timeout {
            Some(timeout) => match self.responses.recv_timeout(timeout) {
                Ok(result) => return map_parsed_plugin_response(result, courier_name, expected_id),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            },
            None => self.responses.recv().ok(),
        };
        match received {
            Some(result) => map_parsed_plugin_response(result, courier_name, expected_id),
            None => Err(CourierError::PluginProtocol {
                courier: courier_name.to_string(),
                message: "plugin produced no response".to_string(),
            }),
        }
    }
}

pub(super) fn spawn_plugin_response_reader(
    stdout: ChildStdout,
    courier_name: String,
) -> mpsc::Receiver<Result<ParsedPluginResponse, CourierError>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let result = read_plugin_response(&mut reader, &courier_name);
            let should_break = result.is_err();
            if tx.send(result).is_err() {
                break;
            }
            if should_break {
                break;
            }
        }
    });
    rx
}

pub(super) fn read_plugin_response<R: std::io::BufRead>(
    reader: &mut R,
    courier_name: &str,
) -> Result<ParsedPluginResponse, CourierError> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|source| CourierError::ReadPluginResponse {
            courier: courier_name.to_string(),
            source,
        })?;
    if bytes == 0 {
        return Err(CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: "plugin produced no response".to_string(),
        });
    }
    parse_jsonrpc_message(line.trim_end()).map_err(|message| CourierError::PluginProtocol {
        courier: courier_name.to_string(),
        message,
    })
}

pub(super) fn read_expected_plugin_response<R: std::io::BufRead>(
    reader: &mut R,
    courier_name: &str,
    expected_id: &PluginRequestId,
) -> Result<PluginResponse, CourierError> {
    map_parsed_plugin_response(
        read_plugin_response(reader, courier_name),
        courier_name,
        expected_id,
    )?
    .ok_or_else(|| CourierError::PluginProtocol {
        courier: courier_name.to_string(),
        message: "plugin produced no response".to_string(),
    })
}

pub(super) fn read_plugin_run_completion(
    process: &mut PersistentPluginProcess,
    courier_name: &str,
    session_id: &str,
    expected_id: &PluginRequestId,
    run_timeout: Option<(String, Duration)>,
    events: &mut Vec<CourierEvent>,
) -> Result<CourierSession, CourierError> {
    loop {
        let response = process.read_response_timeout(
            courier_name,
            expected_id,
            run_timeout.as_ref().map(|(_, duration)| *duration),
        )?;
        let Some(response) = response else {
            let timeout = run_timeout
                .as_ref()
                .map(|(literal, _)| literal.clone())
                .unwrap_or_else(|| "RUN".to_string());
            return Err(CourierError::RunTimedOut {
                session_id: session_id.to_string(),
                timeout,
            });
        };
        match response {
            PluginResponse::Event { event } => events.push(event),
            PluginResponse::Done { session } => return Ok(session),
            PluginResponse::Error { error } => {
                return Err(CourierError::PluginProtocol {
                    courier: courier_name.to_string(),
                    message: format!("{}: {}", error.code, error.message),
                });
            }
            other => {
                return Err(CourierError::PluginProtocol {
                    courier: courier_name.to_string(),
                    message: format!(
                        "unexpected plugin response for `run`: {}",
                        describe_plugin_response(&other)
                    ),
                });
            }
        }
    }
}

pub(super) fn shutdown_persistent_plugin_process(
    process: &mut PersistentPluginProcess,
    courier_name: &str,
    protocol_version: u32,
) -> Result<(), CourierError> {
    if let Ok(request_id) =
        process.write_request(protocol_version, courier_name, PluginRequest::Shutdown)
    {
        let _ = process.read_response_timeout(
            courier_name,
            &request_id,
            Some(Duration::from_millis(200)),
        );
    }
    let _ = process.stdin.flush();
    if process.child.try_wait().ok().flatten().is_none() {
        let _ = process.child.kill();
    }
    let mut stderr = String::new();
    use std::io::Read as _;
    let _ = process.stderr.read_to_string(&mut stderr);
    process
        .child
        .wait()
        .map_err(|source| CourierError::WaitPlugin {
            courier: courier_name.to_string(),
            source,
        })?;
    Ok(())
}

fn map_parsed_plugin_response(
    result: Result<ParsedPluginResponse, CourierError>,
    courier_name: &str,
    expected_id: &PluginRequestId,
) -> Result<Option<PluginResponse>, CourierError> {
    let (received_id, response) = result?;
    if matches!(response, PluginResponse::Event { .. }) {
        return Ok(Some(response));
    }

    let Some(received_id) = received_id else {
        return Err(CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: "plugin response omitted JSON-RPC id".to_string(),
        });
    };
    if &received_id != expected_id {
        return Err(CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: format!(
                "plugin response id `{}` did not match request id `{}`",
                format_request_id(&received_id),
                format_request_id(expected_id)
            ),
        });
    }
    Ok(Some(response))
}

fn format_request_id(id: &PluginRequestId) -> String {
    match id {
        PluginRequestId::String(value) => format!("\"{value}\""),
        PluginRequestId::Integer(value) => value.to_string(),
    }
}

pub(super) fn wait_for_plugin_exit(
    mut child: Child,
    courier_name: &str,
) -> Result<(), CourierError> {
    let mut stderr = String::new();
    if let Some(mut stderr_pipe) = child.stderr.take() {
        use std::io::Read as _;
        stderr_pipe.read_to_string(&mut stderr).map_err(|source| {
            CourierError::ReadPluginResponse {
                courier: courier_name.to_string(),
                source,
            }
        })?;
    }
    let status = child.wait().map_err(|source| CourierError::WaitPlugin {
        courier: courier_name.to_string(),
        source,
    })?;
    if status.success() {
        return Ok(());
    }

    Err(CourierError::PluginExit {
        courier: courier_name.to_string(),
        status: status.code().unwrap_or(-1),
        stderr: stderr.trim().to_string(),
    })
}

pub(super) fn canonical_parcel_dir(parcel: &LoadedParcel) -> Result<String, CourierError> {
    parcel
        .parcel_dir
        .canonicalize()
        .map(|path| path.display().to_string())
        .map_err(|source| CourierError::ReadFile {
            path: parcel.parcel_dir.display().to_string(),
            source,
        })
}

pub(super) fn describe_plugin_response(response: &PluginResponse) -> &'static str {
    match response {
        PluginResponse::Capabilities { .. } => "capabilities",
        PluginResponse::Inspection { .. } => "inspection",
        PluginResponse::Session { .. } => "session",
        PluginResponse::Ok => "ok",
        PluginResponse::Event { .. } => "event",
        PluginResponse::Done { .. } => "done",
        PluginResponse::Error { .. } => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::read_expected_plugin_response;
    use crate::courier::CourierError;
    use crate::plugin_protocol::{PluginRequestId, PluginResponse, response_to_jsonrpc};
    use std::io::Cursor;

    #[test]
    fn read_plugin_response_accepts_eof_terminated_json() {
        let line = response_to_jsonrpc(&PluginRequestId::Integer(1), &PluginResponse::Ok).unwrap();
        let mut reader = Cursor::new(line.into_bytes());

        let response =
            read_expected_plugin_response(&mut reader, "demo", &PluginRequestId::Integer(1))
                .unwrap();

        assert_eq!(response, PluginResponse::Ok);
    }

    #[test]
    fn read_plugin_response_rejects_mismatched_jsonrpc_id() {
        let line = response_to_jsonrpc(&PluginRequestId::Integer(2), &PluginResponse::Ok).unwrap();
        let mut reader = Cursor::new(line.into_bytes());

        let error =
            read_expected_plugin_response(&mut reader, "demo", &PluginRequestId::Integer(1))
                .expect_err("expected mismatched id to fail");

        match error {
            CourierError::PluginProtocol { message, .. } => {
                assert!(message.contains("did not match request id"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
