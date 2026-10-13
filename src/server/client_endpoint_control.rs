pub(crate) fn response(kind: &str, data: String) -> Option<crate::protocol::ServerMessage> {
    (kind == crate::protocol::endpoint::HEALTH_PING_KIND).then(|| {
        crate::protocol::ServerMessage::EndpointControl {
            kind: crate::protocol::endpoint::HEALTH_PONG_KIND.into(),
            data,
        }
    })
}

/// Fork: answers a client shell host panel request on its own thread, so the
/// reader thread keeps reading and the App thread never waits on host probes.
/// One request per connection runs at a time; a second one is answered with
/// `host_facts_busy`. Malformed requests are ignored.
pub(crate) fn answer_host_facts(
    data: &str,
    writer: &super::client_transport::ClientControlWriter,
    in_flight: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    answer_host_facts_with(data, writer, in_flight, crate::host_facts::api_response);
}

fn answer_host_facts_with(
    data: &str,
    writer: &super::client_transport::ClientControlWriter,
    in_flight: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    respond: fn(String, bool) -> String,
) {
    use std::sync::atomic::Ordering;

    const MAX_REQUEST_BYTES: usize = 1024;
    if data.len() > MAX_REQUEST_BYTES {
        tracing::debug!("ignoring oversized host facts request");
        return;
    }
    let Ok(request) =
        serde_json::from_str::<crate::protocol::endpoint::EndpointHostFactsRequest>(data)
    else {
        tracing::debug!("ignoring malformed host facts request");
        return;
    };
    if request.id.is_empty()
        || request.id.len() > crate::server::client_commands::MAX_ENDPOINT_REQUEST_ID_BYTES
    {
        tracing::debug!("ignoring host facts request with an invalid id");
        return;
    }
    if in_flight.swap(true, Ordering::AcqRel) {
        send_host_facts_response(
            writer,
            crate::server::client_commands::error_response(
                request.id,
                "host_facts_busy",
                "a host facts request is already running",
            ),
        );
        return;
    }
    let request_id = request.id.clone();
    let reply_writer = writer.clone();
    let reply_in_flight = std::sync::Arc::clone(in_flight);
    let spawned = crate::thread_spawn::spawn_named("herdr-host-facts-reply", move || {
        let response = respond(request.id, request.refresh);
        reply_in_flight.store(false, Ordering::Release);
        send_host_facts_response(&reply_writer, response);
    });
    if let Err(err) = spawned {
        in_flight.store(false, Ordering::Release);
        tracing::warn!(err = %err, "failed to spawn host facts reply thread");
        send_host_facts_response(
            writer,
            crate::server::client_commands::error_response(
                request_id,
                "server_unavailable",
                "could not start the host facts reply",
            ),
        );
    }
}

fn send_host_facts_response(writer: &super::client_transport::ClientControlWriter, data: String) {
    let message = crate::protocol::ServerMessage::EndpointControl {
        kind: crate::protocol::endpoint::HOST_FACTS_RESPONSE_KIND.into(),
        data,
    };
    let mut framed = Vec::new();
    if crate::protocol::write_message(&mut framed, &message).is_ok() {
        let _ = writer.send(framed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ping_echoes_a_pong_without_entering_server_state() {
        let pong = response(crate::protocol::endpoint::HEALTH_PING_KIND, "probe".into()).unwrap();
        assert!(matches!(
            pong,
            crate::protocol::ServerMessage::EndpointControl { kind, data }
                if kind == crate::protocol::endpoint::HEALTH_PONG_KIND && data == "probe"
        ));
        assert!(response("future.control", String::new()).is_none());
    }

    fn slow_reply(id: String, refresh: bool) -> String {
        std::thread::sleep(std::time::Duration::from_millis(200));
        format!(r#"{{"id":"{id}","refresh":{refresh}}}"#)
    }

    fn decode(frame: Vec<u8>) -> (String, String) {
        let message: crate::protocol::ServerMessage =
            crate::protocol::read_message(&mut frame.as_slice(), 1 << 20).unwrap();
        let crate::protocol::ServerMessage::EndpointControl { kind, data } = message else {
            panic!("expected endpoint control");
        };
        (kind, data)
    }

    #[test]
    fn host_facts_reply_runs_off_the_reader_and_rejects_overlap() {
        let (control_tx, control_rx) = std::sync::mpsc::channel();
        let (render_tx, _render_rx) = std::sync::mpsc::sync_channel(1);
        let writer =
            crate::server::client_transport::ClientWriter::test_channel(control_tx, render_tx);
        let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let started = std::time::Instant::now();
        answer_host_facts_with(
            r#"{"id":"host-facts:1","refresh":true}"#,
            &writer.control,
            &in_flight,
            slow_reply,
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(150),
            "the reader thread must not wait for the reply"
        );
        // A second request while the first runs is answered busy at once.
        answer_host_facts_with(
            r#"{"id":"host-facts:2"}"#,
            &writer.control,
            &in_flight,
            slow_reply,
        );
        // Malformed or oversized requests are ignored.
        answer_host_facts_with("not json", &writer.control, &in_flight, slow_reply);
        answer_host_facts_with(&"x".repeat(2048), &writer.control, &in_flight, slow_reply);

        let timeout = std::time::Duration::from_secs(5);
        let (kind, busy) = decode(control_rx.recv_timeout(timeout).unwrap());
        assert_eq!(kind, crate::protocol::endpoint::HOST_FACTS_RESPONSE_KIND);
        assert!(
            busy.contains("host_facts_busy") && busy.contains("host-facts:2"),
            "{busy}"
        );
        let (_, reply) = decode(control_rx.recv_timeout(timeout).unwrap());
        assert_eq!(reply, r#"{"id":"host-facts:1","refresh":true}"#);
        assert!(control_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        assert!(!in_flight.load(std::sync::atomic::Ordering::Acquire));
    }
}
