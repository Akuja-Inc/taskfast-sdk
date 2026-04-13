//! Deterministic exit-code taxonomy — stable contract for agent orchestrators.
//!
//! Orchestrators (systemd timers, CI runners, other agents) branch on exit
//! code alone: `0` = go, `4` = sleep and retry, `3`/`7` = fix config. Changing
//! a code is a breaking change, so the numeric values are pinned and the
//! mapping from [`crate::cmd::CmdError`] is exercised by tests.
//!
//! Mirrors the plan's taxonomy:
//!
//! | Code | Class                                       |
//! |------|---------------------------------------------|
//! | 0    | success                                     |
//! | 2    | usage / argument error (clap emits this)    |
//! | 3    | auth (401/403)                              |
//! | 4    | rate-limited (429)                          |
//! | 5    | wallet / chain / signing                    |
//! | 6    | server (5xx after retries) / network        |
//! | 7    | validation (422, 4xx other)                 |
//! | 70   | unimplemented (transitional, scaffold-only) |

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success = 0,
    Usage = 2,
    Auth = 3,
    RateLimited = 4,
    Wallet = 5,
    Server = 6,
    Validation = 7,
    Unimplemented = 70,
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        std::process::ExitCode::from(code as u8)
    }
}

impl ExitCode {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}
