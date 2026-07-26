//! Workspace integrity monitor.
//!
//! Detects unauthorized modification of identity files (SOUL.md, AGENTS.md,
//! IDENTITY.md, USER.md, HEARTBEAT.md) via SHA-256 baseline comparison.
//!
//! Inspired by [clawsec/soul-guardian](https://github.com/prompt-security/clawsec).
//!
//! # Design
//!
//! - On startup or `init()`: snapshot SHA-256 hashes of monitored workspace files.
//! - On heartbeat or explicit `check()`: recompute and compare.
//! - Configurable per-file policy: `Restore` (auto-restore from baseline),
//!   `Alert` (warn the user), `Ignore`.
//! - Hash-chained audit log for tamper evidence.
//! - Baselines and approved snapshots stored under `~/.ironclaw/integrity/`.
//! - Symlinks are rejected (same security posture as logseq.rs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::workspace::Workspace;

/// Per-file protection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionMode {
    /// Auto-restore from approved baseline on drift.
    Restore,
    /// Alert the user on drift (no auto-restore).
    Alert,
    /// Ignore changes to this file.
    Ignore,
}

impl Default for ProtectionMode {
    fn default() -> Self {
        Self::Alert
    }
}

/// A file being monitored with its SHA-256 baseline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileBaseline {
    /// Relative path within the workspace (e.g. "SOUL.md").
    pub path: String,
    /// SHA-256 hex digest of the approved content.
    pub sha256: String,
    /// Protection mode for this file.
    pub mode: ProtectionMode,
    /// When the baseline was last approved.
    pub approved_at: String,
}

/// Default files to monitor and their protection modes.
const DEFAULT_TARGETS: &[(&str, ProtectionMode)] = &[
    ("SOUL.md", ProtectionMode::Restore),
    ("AGENTS.md", ProtectionMode::Restore),
    ("IDENTITY.md", ProtectionMode::Alert),
    ("USER.md", ProtectionMode::Alert),
    ("HEARTBEAT.md", ProtectionMode::Alert),
    ("MEMORY.md", ProtectionMode::Ignore),
];

/// A detected integrity violation.
#[derive(Debug, Clone)]
pub struct IntegrityViolation {
    /// Workspace-relative path of the modified file.
    pub file: String,
    /// Expected SHA-256 hash.
    pub expected_hash: String,
    /// Actual SHA-256 hash of current content.
    pub actual_hash: String,
    /// The protection mode for this file.
    pub mode: ProtectionMode,
    /// Whether the file was auto-restored.
    pub restored: bool,
}

impl std::fmt::Display for IntegrityViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self.mode {
            ProtectionMode::Restore if self.restored => "auto-restored",
            ProtectionMode::Restore => "restore failed",
            ProtectionMode::Alert => "DRIFT DETECTED",
            ProtectionMode::Ignore => "ignored",
        };
        write!(
            f,
            "[{}] {} — expected {}, got {}",
            action,
            self.file,
            &self.expected_hash[..12],
            &self.actual_hash[..12],
        )
    }
}

/// An entry in the hash-chained audit log.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub event: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Hash chain: { prev, hash }.
    pub chain: AuditChain,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditChain {
    pub prev: String,
    pub hash: String,
}

/// Workspace integrity monitor.
pub struct IntegrityMonitor {
    /// SHA-256 baselines keyed by workspace-relative path.
    baselines: HashMap<String, FileBaseline>,
    /// Approved file contents (for restore mode).
    approved_snapshots: HashMap<String, Vec<u8>>,
    /// Storage directory for baselines and audit log.
    state_dir: PathBuf,
    /// Hash chain: the most recent audit log hash.
    last_audit_hash: String,
}

/// Genesis hash for the first audit entry.
const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

impl IntegrityMonitor {
    /// Create a new integrity monitor.
    ///
    /// `state_dir` is typically `~/.ironclaw/integrity/`.
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            baselines: HashMap::new(),
            approved_snapshots: HashMap::new(),
            state_dir,
            last_audit_hash: GENESIS_HASH.to_string(),
        }
    }

    /// Default state directory.
    pub fn default_state_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ironclaw")
            .join("integrity")
    }

    /// Initialize baselines from the workspace.
    ///
    /// Reads each monitored file, computes SHA-256, stores as baseline.
    /// Call on first setup or when the user approves current contents.
    pub async fn init(&mut self, workspace: &Workspace) -> Result<usize, String> {
        let mut count = 0;

        for (path, mode) in DEFAULT_TARGETS {
            if *mode == ProtectionMode::Ignore {
                continue;
            }

            match workspace.read(path).await {
                Ok(content) => {
                    let hash = sha256_hex(content.content.as_bytes());
                    let now = chrono::Utc::now().to_rfc3339();

                    self.baselines.insert(
                        path.to_string(),
                        FileBaseline {
                            path: path.to_string(),
                            sha256: hash.clone(),
                            mode: *mode,
                            approved_at: now.clone(),
                        },
                    );

                    if *mode == ProtectionMode::Restore {
                        self.approved_snapshots
                            .insert(path.to_string(), content.content.as_bytes().to_vec());
                    }

                    self.append_audit("baseline_set", path, None, Some(&hash), Some("init"));
                    count += 1;
                }
                Err(_) => {
                    tracing::debug!("Integrity: file {} not found in workspace, skipping", path);
                }
            }
        }

        // Persist baselines
        self.save_baselines()?;
        self.save_approved_snapshots()?;

        tracing::info!("Integrity monitor initialized with {} baselines", count);
        Ok(count)
    }

    /// Check all monitored files for drift.
    ///
    /// Returns a list of violations (empty = all good).
    pub async fn check(&mut self, workspace: &Workspace) -> Vec<IntegrityViolation> {
        let mut violations = Vec::new();

        for (path, baseline) in &self.baselines.clone() {
            if baseline.mode == ProtectionMode::Ignore {
                continue;
            }

            let current_hash = match workspace.read(path).await {
                Ok(content) => sha256_hex(content.content.as_bytes()),
                Err(_) => {
                    // File deleted — treat as drift
                    violations.push(IntegrityViolation {
                        file: path.clone(),
                        expected_hash: baseline.sha256.clone(),
                        actual_hash: "FILE_DELETED".to_string(),
                        mode: baseline.mode,
                        restored: false,
                    });
                    continue;
                }
            };

            if current_hash == baseline.sha256 {
                continue; // No drift
            }

            tracing::warn!(
                "Integrity drift: {} expected {} got {}",
                path,
                &baseline.sha256[..12],
                &current_hash[..12]
            );

            let mut restored = false;

            if baseline.mode == ProtectionMode::Restore {
                if let Some(approved) = self.approved_snapshots.get(path) {
                    let content = String::from_utf8_lossy(approved).to_string();
                    match workspace.write(path, &content).await {
                        Ok(_) => {
                            restored = true;
                            tracing::info!("Integrity: auto-restored {}", path);
                        }
                        Err(e) => {
                            tracing::error!("Integrity: failed to restore {}: {}", path, e);
                        }
                    }
                }
            }

            self.append_audit(
                "drift_detected",
                path,
                Some(&baseline.sha256),
                Some(&current_hash),
                if restored { Some("auto_restored") } else { Some("alert") },
            );

            violations.push(IntegrityViolation {
                file: path.clone(),
                expected_hash: baseline.sha256.clone(),
                actual_hash: current_hash,
                mode: baseline.mode,
                restored,
            });
        }

        violations
    }

    /// Approve the current contents of a file as the new baseline.
    pub async fn approve(&mut self, workspace: &Workspace, path: &str) -> Result<(), String> {
        let content = workspace
            .read(path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", path, e))?;

        let hash = sha256_hex(content.content.as_bytes());
        let now = chrono::Utc::now().to_rfc3339();

        let mode = self
            .baselines
            .get(path)
            .map(|b| b.mode)
            .unwrap_or(ProtectionMode::Alert);

        self.baselines.insert(
            path.to_string(),
            FileBaseline {
                path: path.to_string(),
                sha256: hash.clone(),
                mode,
                approved_at: now,
            },
        );

        if mode == ProtectionMode::Restore {
            self.approved_snapshots
                .insert(path.to_string(), content.content.as_bytes().to_vec());
        }

        self.append_audit("baseline_approved", path, None, Some(&hash), Some("approve"));
        self.save_baselines()?;
        self.save_approved_snapshots()?;

        Ok(())
    }

    /// Get a summary of current protection status.
    pub fn status(&self) -> Vec<(String, String, ProtectionMode)> {
        self.baselines
            .values()
            .map(|b| (b.path.clone(), b.sha256[..12].to_string(), b.mode))
            .collect()
    }

    /// Load baselines from disk.
    pub fn load(&mut self) -> Result<(), String> {
        let baselines_path = self.state_dir.join("baselines.json");
        if baselines_path.exists() {
            let data =
                std::fs::read_to_string(&baselines_path).map_err(|e| format!("Read error: {}", e))?;
            let baselines: HashMap<String, FileBaseline> =
                serde_json::from_str(&data).map_err(|e| format!("Parse error: {}", e))?;
            self.baselines = baselines;
        }

        // Load approved snapshots
        let approved_dir = self.state_dir.join("approved");
        if approved_dir.is_dir() {
            for baseline in self.baselines.values() {
                if baseline.mode == ProtectionMode::Restore {
                    let snap_path = approved_dir.join(&baseline.path);
                    if snap_path.is_file() {
                        if let Ok(data) = std::fs::read(&snap_path) {
                            self.approved_snapshots.insert(baseline.path.clone(), data);
                        }
                    }
                }
            }
        }

        // Load last audit hash
        let audit_path = self.state_dir.join("audit.jsonl");
        if audit_path.is_file() {
            if let Ok(data) = std::fs::read_to_string(&audit_path) {
                if let Some(last_line) = data.lines().rev().find(|l| !l.trim().is_empty()) {
                    if let Ok(entry) = serde_json::from_str::<AuditEntry>(last_line) {
                        self.last_audit_hash = entry.chain.hash;
                    }
                }
            }
        }

        Ok(())
    }

    // --- Private helpers ---

    fn save_baselines(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.state_dir).map_err(|e| format!("mkdir error: {}", e))?;
        let data = serde_json::to_string_pretty(&self.baselines)
            .map_err(|e| format!("Serialize error: {}", e))?;
        let path = self.state_dir.join("baselines.json");
        atomic_write(&path, data.as_bytes())
    }

    fn save_approved_snapshots(&self) -> Result<(), String> {
        let dir = self.state_dir.join("approved");
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir error: {}", e))?;

        for (path, content) in &self.approved_snapshots {
            let full_path = dir.join(path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir error: {}", e))?;
            }
            atomic_write(&full_path, content)?;
        }
        Ok(())
    }

    fn append_audit(
        &mut self,
        event: &str,
        file: &str,
        old_hash: Option<&str>,
        new_hash: Option<&str>,
        action: Option<&str>,
    ) {
        let entry_without_chain = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "event": event,
            "file": file,
            "old_hash": old_hash,
            "new_hash": new_hash,
            "action": action,
        });

        // Hash chain: sha256(prev_hash + "\n" + canonical_json)
        let canonical = serde_json::to_string(&entry_without_chain).unwrap_or_default();
        let payload = format!("{}\n{}", self.last_audit_hash, canonical);
        let hash = sha256_hex(payload.as_bytes());

        let entry = AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event: event.to_string(),
            file: file.to_string(),
            old_hash: old_hash.map(String::from),
            new_hash: new_hash.map(String::from),
            action: action.map(String::from),
            chain: AuditChain {
                prev: self.last_audit_hash.clone(),
                hash: hash.clone(),
            },
        };

        self.last_audit_hash = hash;

        // Append to audit log
        let audit_path = self.state_dir.join("audit.jsonl");
        if let Ok(line) = serde_json::to_string(&entry) {
            let _ = std::fs::create_dir_all(&self.state_dir);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&audit_path);
            if let Ok(ref mut f) = file {
                use std::io::Write;
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}

/// Compute SHA-256 hex digest of bytes.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Atomic write: write to tmp file, then rename.
fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("Write error: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Rename error: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex() {
        let hash = sha256_hex(b"hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_default_targets() {
        // SOUL.md and AGENTS.md should be Restore
        assert_eq!(DEFAULT_TARGETS[0], ("SOUL.md", ProtectionMode::Restore));
        assert_eq!(DEFAULT_TARGETS[1], ("AGENTS.md", ProtectionMode::Restore));
        // MEMORY.md should be Ignore
        assert_eq!(DEFAULT_TARGETS[5], ("MEMORY.md", ProtectionMode::Ignore));
    }

    #[test]
    fn test_audit_chain() {
        let mut monitor = IntegrityMonitor::new(std::env::temp_dir().join("ironclaw_test_integrity"));
        assert_eq!(monitor.last_audit_hash, GENESIS_HASH);

        monitor.append_audit("test", "SOUL.md", None, Some("abc123"), Some("init"));
        assert_ne!(monitor.last_audit_hash, GENESIS_HASH);

        let prev = monitor.last_audit_hash.clone();
        monitor.append_audit("test2", "AGENTS.md", None, Some("def456"), Some("init"));
        assert_ne!(monitor.last_audit_hash, prev);
    }
}
