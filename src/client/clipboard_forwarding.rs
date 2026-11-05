use super::*;

pub(super) fn decode_clipboard_payload(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Returns true when the caller should show copy feedback. A pane repeating text
/// the clipboard already holds gets none, so it can't dismiss a supervisor notice.
pub(super) fn forward_clipboard(data: &str) -> bool {
    let Some(bytes) = decode_clipboard_payload(data) else {
        warn!("received invalid clipboard payload from server");
        return false;
    };
    crate::selection::write_osc52_bytes(&bytes).dispatched()
}
