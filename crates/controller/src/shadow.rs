//! Shadow persistence — survive a controller-only restart without losing the
//! ability to prune orphaned Sōzu state.
//!
//! The shadow (last-applied IR) lives in memory and normally resets to empty
//! when the controller process restarts. Both containers share an `emptyDir`, so
//! if *only* the controller restarts, Sōzu keeps its live state but the
//! controller would re-add everything from an empty baseline and never compute
//! the removes for objects deleted meanwhile.
//!
//! So we persist the shadow to that shared volume and reload it on startup — but
//! only when Sōzu *still holds the state it describes*. If Sōzu itself restarted
//! (empty), the persisted shadow is stale and trusting it would leave a fresh
//! Sōzu unprogrammed; in that case we start empty and re-apply everything. Any
//! error falls back to empty too, because re-applying is always correct.

use sozu_gw_agent::SozuAgentHandle;
use sozu_gw_ir::Ir;
use tracing::{debug, info, warn};

/// Load the initial shadow. Returns the persisted last-applied IR only when it
/// is safe to trust (file present AND Sōzu non-empty); otherwise an empty IR.
pub async fn load_initial(agent: &SozuAgentHandle, shadow_file: &str, probe_file: &str) -> Ir {
    if shadow_file.is_empty() {
        return Ir::default();
    }
    let raw = match std::fs::read_to_string(shadow_file) {
        Ok(s) => s,
        Err(e) => {
            debug!(error = %e, file = %shadow_file, "no persisted shadow; starting empty");
            return Ir::default();
        }
    };
    // The persisted shadow is only trustworthy if Sōzu still has its state.
    match sozu_has_state(agent, probe_file).await {
        Ok(true) => {}
        Ok(false) => {
            info!("Sōzu state is empty (restarted?); ignoring persisted shadow, will re-apply");
            return Ir::default();
        }
        Err(e) => {
            warn!(error = %e, "could not probe Sōzu state; ignoring persisted shadow, will re-apply");
            return Ir::default();
        }
    }
    match serde_json::from_str::<Ir>(&raw) {
        Ok(ir) => {
            info!(file = %shadow_file, "resumed shadow from persisted state");
            ir
        }
        Err(e) => {
            warn!(error = %e, "persisted shadow is unreadable; will re-apply");
            Ir::default()
        }
    }
}

/// Probe whether Sōzu currently holds any routing state, by asking it to dump to
/// `probe_file` (on the shared volume) and checking the dump is non-empty.
async fn sozu_has_state(
    agent: &SozuAgentHandle,
    probe_file: &str,
) -> Result<bool, sozu_gw_agent::SozuError> {
    agent.save_state(probe_file.to_string()).await?;
    let dump = std::fs::read_to_string(probe_file).unwrap_or_default();
    let _ = std::fs::remove_file(probe_file); // best-effort cleanup
    Ok(state_dump_is_nonempty(&dump))
}

/// A Sōzu state dump is newline/NUL-delimited JSON records; it is non-empty when
/// it has at least one record.
fn state_dump_is_nonempty(dump: &str) -> bool {
    dump.split('\n')
        .any(|line| !line.trim_matches(['\0', ' ', '\r', '\t']).is_empty())
}

/// Persist the shadow. Best-effort: a write failure must never fail a reconcile.
pub fn persist(shadow_file: &str, shadow: &Ir) {
    if shadow_file.is_empty() {
        return;
    }
    match serde_json::to_vec(shadow) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(shadow_file, bytes) {
                warn!(error = %e, file = %shadow_file, "failed to persist shadow");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize shadow"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dump_is_detected_as_empty() {
        assert!(!state_dump_is_nonempty(""));
        assert!(!state_dump_is_nonempty("\n\0\n"));
        assert!(!state_dump_is_nonempty("   \r\n"));
    }

    #[test]
    fn nonempty_dump_is_detected() {
        assert!(state_dump_is_nonempty(
            "{\"id\":\"SAVE-0\",\"content\":{}}\n\0"
        ));
    }

    #[test]
    fn shadow_round_trips_through_json() {
        // A representative IR must survive serialize -> deserialize unchanged, so
        // a resumed shadow diffs cleanly against a freshly-built desired IR.
        let ir = Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::LeastLoaded,
                sticky_session: true,
                https_redirect: false,
                max_connections_per_ip: Some(100),
                retry_after: Some(5),
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&ir).unwrap();
        let back: Ir = serde_json::from_str(&json).unwrap();
        assert_eq!(ir, back);
    }
}
