use std::time::Duration;

pub const REKEY_AFTER_TIME: Duration = Duration::from_secs(120);
pub const RESPONDER_REKEY_TIME: Duration = Duration::from_secs(165);
pub const REJECT_AFTER_TIME: Duration = Duration::from_secs(180);
pub const REKEY_AFTER_MESSAGES: u64 = 1 << 60;
pub const REKEY_TIMEOUT: Duration = Duration::from_secs(5);
pub const REKEY_TIMEOUT_JITTER_MS: u64 = 333;
pub const UNREACHABLE_AFTER: Duration = Duration::from_secs(90);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);
pub const KEEPALIVE_JITTER_MS: u64 = 5000;
pub const KEEPALIVE_MAX_BLOCKS: usize = 7;
pub const MAX_QUEUED_PACKETS: usize = 128;
