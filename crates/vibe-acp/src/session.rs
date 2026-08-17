//! Everything one ACP session is made of: the settings it negotiates, the
//! live harness that runs it, the roots it is opened over, and the canonical
//! payloads it is rebuilt from.

pub(crate) mod harness;
pub(crate) mod paths;
pub(crate) mod settings;
pub(crate) mod wire;

pub(crate) use harness::{AcpHarness, ActivePhase};
pub(crate) use paths::{
    ensure_matching_cwd, require_absolute_cwd, same_path, validate_session_paths,
};
pub(crate) use settings::{
    Mode, SessionSettings, Thinking, session_options, thinking_config_options,
};
pub(crate) use wire::{
    acp_session_info, decode_session_cursor, encode_session_cursor, metadata_session_id,
};

/// Wall-clock milliseconds, used where an identifier has to stay unique across
/// processes rather than only within one.
pub(crate) fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}
