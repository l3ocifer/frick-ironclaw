//! Safety layer for prompt injection defense.
//!
//! New code should import directly from `ironclaw_safety`.

// homelab security hardening (ported onto upstream safety module)
pub mod command_guard;
pub mod integrity;
