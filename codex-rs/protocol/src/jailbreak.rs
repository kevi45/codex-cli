//! Jailbreak level state management.
//!
//! Three levels:
//!  - `Disabled`: no content-handling section, no synthetic history
//!  - `Lowered`:  softer content-handling section, empty synthetic history
//!  - `Full`:     strongest content-handling section + full synthetic history
//!
//! Defaults to `Full` when not explicitly set.
//!
//! This lives in `codex-protocol` so both `codex-core` (which builds the
//! prompt) and `codex-tui` (which provides the `/jailbreak` command) can
//! access the shared atomic state.

use std::fmt;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

/// The three jailbreak levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailbreakLevel {
    Disabled = 0,
    Lowered = 1,
    Full = 2,
}

impl fmt::Display for JailbreakLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JailbreakLevel::Disabled => write!(f, "disabled"),
            JailbreakLevel::Lowered => write!(f, "lowered"),
            JailbreakLevel::Full => write!(f, "full"),
        }
    }
}

impl JailbreakLevel {
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "disabled" => Some(JailbreakLevel::Disabled),
            "lowered" => Some(JailbreakLevel::Lowered),
            "full" => Some(JailbreakLevel::Full),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            JailbreakLevel::Disabled => "Disabled",
            JailbreakLevel::Lowered => "Lowered",
            JailbreakLevel::Full => "Full",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            JailbreakLevel::Disabled => "No content-handling overrides or synthetic history",
            JailbreakLevel::Lowered => {
                "Softer content-handling section, no synthetic history (default)"
            }
            JailbreakLevel::Full => {
                "Strongest content-handling section + full synthetic history priming"
            }
        }
    }

    pub const ALL: [JailbreakLevel; 3] = [
        JailbreakLevel::Disabled,
        JailbreakLevel::Lowered,
        JailbreakLevel::Full,
    ];
}

// Global atomic state — default is Full (2).
static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(2);

pub fn get_jailbreak_level() -> JailbreakLevel {
    match CURRENT_LEVEL.load(Ordering::Relaxed) {
        0 => JailbreakLevel::Disabled,
        2 => JailbreakLevel::Full,
        _ => JailbreakLevel::Lowered,
    }
}

pub fn set_jailbreak_level(level: JailbreakLevel) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}
