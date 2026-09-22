//! Shadow persistence — survive a controller-only restart without losing the
//! ability to prune orphaned Sōzu state.
//!
//! The shadow (last-applied IR) lives in memory and normally resets to empty
//! when the controller process restarts. Both containers share an `emptyDir`, so
//! if *only* the controller restarts, Sōzu keeps its live state but the
//! controller would re-add everything from an empty baseline and never compute
//! the removes for objects deleted meanwhile.
//!
//! So we persist the shadow to that shared volume, **together with the restart
//! generation of the Sōzu that holds it** (its command-socket identity and live
//! worker PIDs), and reload it on startup only when the Sōzu we find answers
//! with the same socket identity. A Sōzu that restarted while the controller
//! was down recreated its socket, so its generation differs and the persisted
//! shadow is ignored: we start empty and re-apply everything. Any read or parse
//! error falls back to empty too, because re-applying is always correct.
//!
//! An emptiness probe (`save_state` and "is the dump empty?") cannot do this
//! job: with the static HTTP/HTTPS listeners of this deployment a fresh Sōzu
//! already dumps four listener records, so such a probe never reads "empty".

use serde::{Deserialize, Serialize};
use sozu_gw_agent::{SozuAgentHandle, SozuError, SozuGeneration};
use sozu_gw_ir::Ir;
use tracing::{debug, info, warn};

/// The last-applied state, paired with the generation of the Sōzu it was
/// applied to. This is also the on-disk format (see [`persist`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Shadow {
    /// The generation of the Sōzu that holds `ir`; `None` until a probe has
    /// succeeded, in which case the shadow is not yet proven and a
    /// non-empty `ir` is reset on the first successful probe.
    #[serde(default)]
    pub generation: Option<SozuGeneration>,
    /// The last successfully applied IR (private keys redacted on disk).
    #[serde(default)]
    pub ir: Ir,
}

impl Shadow {
    pub fn empty(generation: Option<SozuGeneration>) -> Self {
        Self {
            generation,
            ir: Ir::default(),
        }
    }
}

/// Load the initial shadow. Returns the persisted last-applied IR only when it
/// is safe to trust: the file is present and readable, and the Sōzu that
/// answered `current` has the same command socket as the one the file was
/// written against. Worker PIDs may differ — a worker that bounced while the
/// controller was down was re-fed the main process's state, so the shadow
/// still describes what Sōzu serves. `current` being `None` means no proof at
/// all, so nothing is resumed.
pub fn load_initial(shadow_file: &str, current: Option<&SozuGeneration>) -> Shadow {
    let empty = Shadow::empty(current.cloned());
    if shadow_file.is_empty() {
        return empty;
    }
    let raw = match std::fs::read_to_string(shadow_file) {
        Ok(s) => s,
        Err(e) => {
            debug!(error = %e, file = %shadow_file, "no persisted shadow; starting empty");
            return empty;
        }
    };
    let persisted = match serde_json::from_str::<Shadow>(&raw) {
        Ok(persisted) => persisted,
        Err(e) => {
            // Also the downgrade path: the IR is a versioned-by-nothing serde
            // enum soup, so a shadow written by a newer controller can carry a
            // variant this one has no name for. Falling back to empty is safe
            // for traffic (everything is re-applied) but loses the baseline, so
            // orphans from before the downgrade are not pruned until an object
            // changes. Widening an IR enum is therefore a compatibility event,
            // not a refactor.
            warn!(error = %e, "persisted shadow is unreadable; will re-apply");
            return empty;
        }
    };
    match (persisted.generation.as_ref(), current) {
        (Some(written_against), Some(current)) if written_against.socket == current.socket => {
            info!(file = %shadow_file, "resumed shadow from persisted state (same Sōzu socket)");
            Shadow {
                generation: Some(current.clone()),
                ir: persisted.ir,
            }
        }
        (Some(written_against), Some(current)) => {
            info!(
                persisted = ?written_against.socket,
                current = ?current.socket,
                "Sōzu's command socket changed since the shadow was written (restarted while the controller was down?); ignoring persisted shadow, will re-apply"
            );
            empty
        }
        (None, _) => {
            info!("persisted shadow carries no Sōzu generation; ignoring it, will re-apply");
            empty
        }
        (_, None) => {
            warn!("could not read Sōzu's generation; ignoring persisted shadow, will re-apply");
            empty
        }
    }
}

/// Outcome of a restart-generation check, for the caller's control flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationCheck {
    /// The probe succeeded and the generation matches the baseline (or there
    /// was nothing applied to lose); the shadow stands.
    Unchanged,
    /// The probe succeeded, the socket changed under a non-empty shadow,
    /// and the shadow was reset — a full re-apply is due.
    Reset,
    /// The probe failed; nothing was decided. The caller must retry: never
    /// reset (a blind full re-apply) and never conclude "no restart" from an
    /// error.
    ProbeFailed,
}

/// Mid-life counterpart of [`load_initial`]: check Sōzu's *restart generation*
/// against the one the shadow was applied to, and reset the shadow to empty
/// when its command socket changed.
///
/// If the Sōzu container restarts under a live controller (main-process crash;
/// `worker_automatic_restart` only covers workers), it comes back empty while
/// the in-memory shadow still claims everything is applied — the diff stays
/// empty and every request 404s indefinitely. An emptiness probe cannot detect
/// this reliably: any successful add-bearing apply that lands on the restarted
/// Sōzu first (e.g. the tail of the very batch whose reconnect signalled the
/// restart) makes it non-empty again, masking the restart forever. The socket
/// identity is immune to that race: a container restart recreates the command
/// socket even when its new PID namespace reuses the same worker PIDs.
///
/// A changed worker set *alone* does not reset. The main process re-feeds its
/// state to a respawned worker, so the shadow still describes what Sōzu
/// serves; resetting would diff `empty → desired`, which emits only adds — so
/// an object that left the desired state in the meantime would never be
/// removed, and every frontend would be re-added and "repaired" (remove +
/// re-add) for nothing.
///
/// On success the baseline advances to the observed generation. A missing
/// baseline (the startup capture failed) resets too when the shadow is
/// non-empty: with no established generation there is no proof Sōzu still
/// holds what the shadow claims, and one extra full re-apply is the safe way
/// out.
pub async fn check_restart_generation(
    agent: &SozuAgentHandle,
    shadow: &mut Shadow,
) -> GenerationCheck {
    let probe = agent.generation().await;
    if let Err(e) = &probe {
        warn!(error = %e, "could not query Sōzu's generation; keeping the shadow and retrying");
        return GenerationCheck::ProbeFailed;
    }
    let outcome = if should_reset(&probe, shadow.generation.as_ref(), &shadow.ir) {
        warn!(
            baseline = ?shadow.generation,
            current = ?probe.as_ref().ok(),
            "Sōzu's command socket changed (restarted?); resetting the shadow to re-apply the full state"
        );
        shadow.ir = Ir::default();
        GenerationCheck::Reset
    } else {
        GenerationCheck::Unchanged
    };
    if let Ok(generation) = probe {
        if let Some(known) = &shadow.generation {
            if outcome == GenerationCheck::Unchanged && known.worker_pids != generation.worker_pids
            {
                info!(
                    previous = ?known.worker_pids,
                    current = ?generation.worker_pids,
                    "Sōzu's worker set changed on the same socket (worker restart); keeping the shadow"
                );
            }
        }
        shadow.generation = Some(generation);
    }
    outcome
}

/// Pure reset decision, keyed on (probe result, socket change, shadow
/// emptiness). A probe *error* never resets (a transient failure must not
/// trigger a full blind re-apply — the caller retries), and an empty shadow
/// never resets (nothing applied, nothing to lose). On a successful probe the
/// shadow is reset when the command socket differs from the baseline's — a
/// worker bounce on the same socket does not count — or when no baseline was
/// ever established (an unproven generation under a claimed-applied shadow is
/// not trustworthy).
fn should_reset(
    probe: &Result<SozuGeneration, SozuError>,
    baseline: Option<&SozuGeneration>,
    shadow: &Ir,
) -> bool {
    let Ok(generation) = probe else {
        return false;
    };
    if *shadow == Ir::default() {
        return false;
    }
    match baseline {
        Some(known) => known.socket != generation.socket,
        None => true,
    }
}

/// Persist the shadow. Best-effort: a write failure must never fail a reconcile.
///
/// Written atomically — temp file in the same directory, `sync_all`, then
/// `rename` over the target — so a crash mid-write can never leave a truncated
/// JSON behind. A torn shadow would parse-fail on restart and fall back to a
/// full re-apply against a Sōzu that still holds state, which is exactly the
/// divergence this file exists to avoid.
///
/// TLS private keys are stripped before writing: the previous side of a diff
/// only ever needs a certificate's public identity (PEM → fingerprint, SNI
/// names, listener) — Add/Replace requests are always built from the freshly
/// built *desired* IR, whose keys come straight from the Secrets. Persisting
/// keys would put every tenant's private key at rest on the shared volume for
/// zero functional benefit.
pub fn persist(shadow_file: &str, shadow: &Shadow) {
    if shadow_file.is_empty() {
        return;
    }
    let redacted = Shadow {
        generation: shadow.generation.clone(),
        ir: redact_private_keys(&shadow.ir),
    };
    let bytes = match serde_json::to_vec(&redacted) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(error = %e, "failed to serialize shadow");
            return;
        }
    };
    // A predictable sibling name is fine: the directory is a private shared
    // volume and only one controller writes there.
    let tmp_file = format!("{shadow_file}.tmp");
    if let Err(e) = write_atomically(shadow_file, &tmp_file, &bytes) {
        warn!(error = %e, file = %shadow_file, "failed to persist shadow");
        // Best-effort cleanup so a failed attempt leaves no stale temp behind.
        let _ = std::fs::remove_file(&tmp_file);
    }
}

/// The IR with every certificate's private key blanked — what [`persist`]
/// actually writes. A reloaded key-less shadow still identifies its
/// certificates (the diff works on fingerprints computed from the public PEM),
/// so it diffs cleanly against a freshly built desired IR.
fn redact_private_keys(shadow: &Ir) -> Ir {
    let mut redacted = shadow.clone();
    for cert in &mut redacted.certificates {
        cert.key = String::new();
    }
    redacted
}

/// Write `bytes` to `tmp_file`, fsync it, then rename it over `target` (an
/// atomic replace on the same filesystem — the volume the temp name shares).
fn write_atomically(target: &str, tmp_file: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(tmp_file)?;
    file.write_all(bytes)?;
    // Flush to disk before the rename, so the rename can never publish a file
    // whose content is still only in the page cache.
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp_file, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_only_on_a_successful_probe_showing_a_new_generation() {
        let generation = |pids| SozuGeneration {
            socket: sozu_gw_agent::SocketIdentity {
                device: 1,
                inode: 10,
                changed_secs: 1,
                changed_nanos: 0,
            },
            worker_pids: std::collections::BTreeSet::from(pids),
        };
        let applied = Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::default(),
                sticky_session: false,
                https_redirect: false,
                max_connections_per_ip: None,
                retry_after: None,
            }],
            ..Default::default()
        };
        let empty = Ir::default();
        let baseline = generation([101, 102]);

        // Sōzu's main process restarted: it recreated its command socket.
        let mut restarted = generation([201, 202]);
        restarted.socket.inode += 7;
        restarted.socket.changed_secs += 1;
        assert!(should_reset(&Ok(restarted), Some(&baseline), &applied));
        // The same PID set can never mask that: only the socket is compared.
        let mut same_pids_new_socket = baseline.clone();
        same_pids_new_socket.socket.changed_secs += 1;
        assert!(should_reset(
            &Ok(same_pids_new_socket),
            Some(&baseline),
            &applied
        ));
        // A worker bounce on the same socket (`worker_automatic_restart`) does
        // NOT reset: the main process re-feeds its state to the new worker, so
        // the shadow still describes what Sōzu serves — and a reset would diff
        // `empty → desired`, never removing what left the desired state since.
        assert!(!should_reset(
            &Ok(generation([101, 103])),
            Some(&baseline),
            &applied
        ));
        // Container restarts can reuse every PID; a new socket still resets.
        let mut replaced_socket = baseline.clone();
        replaced_socket.socket.inode += 1;
        assert!(should_reset(
            &Ok(replaced_socket),
            Some(&baseline),
            &applied
        ));
        // Even if the filesystem reuses the inode, a later ctime distinguishes it.
        let mut reused_inode = baseline.clone();
        reused_inode.socket.changed_nanos += 1;
        assert!(should_reset(&Ok(reused_inode), Some(&baseline), &applied));
        // Same socket and worker set: a transient reconnect preserves the shadow.
        assert!(!should_reset(
            &Ok(baseline.clone()),
            Some(&baseline),
            &applied
        ));
        // Nothing was ever applied: nothing a restarted Sōzu could have lost.
        let mut restarted_again = generation([201, 202]);
        restarted_again.socket.inode += 1;
        assert!(!should_reset(&Ok(restarted_again), Some(&baseline), &empty));
        // A probe error must never trigger a full blind re-apply — the caller
        // keeps the reconnect pending and retries.
        assert!(!should_reset(
            &Err(sozu_gw_agent::SozuError::WorkerGone),
            Some(&baseline),
            &applied
        ));
        // No baseline was ever captured while the shadow claims applied state:
        // the generation is unproven, so reset (one extra full re-apply).
        assert!(should_reset(&Ok(baseline.clone()), None, &applied));
        // ... but an empty shadow with no baseline is just a fresh start.
        assert!(!should_reset(&Ok(baseline), None, &empty));
    }

    #[tokio::test]
    async fn failed_generation_probe_keeps_the_baseline_and_shadow() {
        let agent =
            SozuAgentHandle::spawn("/nonexistent/sozu-generation.sock").expect("spawn agent");
        let mut baseline = Some(SozuGeneration {
            socket: sozu_gw_agent::SocketIdentity {
                device: 1,
                inode: 10,
                changed_secs: 1,
                changed_nanos: 0,
            },
            worker_pids: std::collections::BTreeSet::from([7, 8]),
        });
        let mut shadow = Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::default(),
                sticky_session: false,
                https_redirect: false,
                max_connections_per_ip: None,
                retry_after: None,
            }],
            ..Default::default()
        };
        let mut state = Shadow {
            generation: baseline.take(),
            ir: shadow.clone(),
        };
        let before = state.clone();
        assert_eq!(
            check_restart_generation(&agent, &mut state).await,
            GenerationCheck::ProbeFailed
        );
        assert_eq!(state, before);
        shadow = state.ir;
        let _ = shadow;
    }

    #[test]
    fn persist_replaces_an_existing_file_and_leaves_no_temp_behind() {
        let file = std::env::temp_dir().join(format!(
            "sozu-gw-shadow-persist-{}.json",
            std::process::id()
        ));
        let path = file.to_str().expect("utf-8 temp path");
        let tmp = format!("{path}.tmp");

        // An existing baseline from a previous apply, plus a stale temp file
        // from a hypothetical earlier crash: persist must replace the former
        // and consume the latter.
        std::fs::write(&file, b"{not even json").expect("seed old shadow");
        std::fs::write(&tmp, b"stale").expect("seed stale temp");

        let ir = Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::default(),
                sticky_session: false,
                https_redirect: true,
                max_connections_per_ip: None,
                retry_after: None,
            }],
            ..Default::default()
        };
        let shadow = Shadow {
            generation: Some(generation_fixture([7, 8])),
            ir,
        };
        persist(path, &shadow);

        let raw = std::fs::read_to_string(&file).expect("shadow file present");
        let back: Shadow = serde_json::from_str(&raw).expect("persisted shadow parses");
        assert_eq!(back, shadow, "the target must hold exactly the new shadow");
        assert!(
            !std::path::Path::new(&tmp).exists(),
            "the temp file must be renamed away, never left behind"
        );

        let _ = std::fs::remove_file(&file);
    }

    /// A real (test-fixture) certificate, so the translator can fingerprint it.
    const CERT_A: &str = include_str!("../../translator/tests/fixtures/cert_a.pem");
    const KEY_A: &str = include_str!("../../translator/tests/fixtures/key_a.pem");

    fn ir_with_certificate() -> Ir {
        Ir {
            certificates: vec![sozu_gw_ir::Certificate {
                listener: "0.0.0.0:8443".parse().expect("addr"),
                certificate: CERT_A.to_string(),
                chain: vec![],
                key: KEY_A.to_string(),
                names: vec!["app.example.com".to_string()],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn persisted_shadow_contains_no_private_key_material() {
        let file =
            std::env::temp_dir().join(format!("sozu-gw-shadow-redact-{}.json", std::process::id()));
        let path = file.to_str().expect("utf-8 temp path");

        persist(
            path,
            &Shadow {
                generation: Some(generation_fixture([7, 8])),
                ir: ir_with_certificate(),
            },
        );

        let raw = std::fs::read_to_string(&file).expect("shadow file present");
        assert!(
            !raw.contains("PRIVATE KEY"),
            "no private-key material may reach the persisted shadow"
        );
        let back: Shadow = serde_json::from_str(&raw).expect("persisted shadow parses");
        assert!(back.ir.certificates[0].key.is_empty());
        assert_eq!(
            back.ir.certificates[0].certificate, CERT_A,
            "the public identity must survive redaction"
        );

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_reloaded_keyless_shadow_diffs_cleanly_against_the_desired_ir() {
        // Simulate a controller restart: the persisted (key-less) shadow is
        // reloaded and diffed against a freshly built IR that carries the key.
        // Nothing changed, so the diff must be empty — proving the previous
        // side never needs the private key.
        let desired = ir_with_certificate();
        let reloaded: Ir =
            serde_json::from_str(&serde_json::to_string(&redact_private_keys(&desired)).unwrap())
                .expect("redacted shadow parses");

        let requests = sozu_gw_translator::reconcile(&reloaded, &desired)
            .expect("a key-less previous side must not fail the diff");
        // `Debug`, deliberately, and not JSON like the translator's golden tests:
        // since sozu-command-lib 2.2.1 it prints the certificate as lengths and
        // counts, which names the request without putting the fixture's private
        // key in the failure output — in the one test whose whole subject is
        // that key material does not escape.
        assert!(
            requests.is_empty(),
            "an unchanged cert must yield no requests: {requests:?}"
        );
    }

    /// An IR written by an older controller must still parse.
    ///
    /// The `ir` half of the shadow file is a bare `Ir` with no version field, so every field added
    /// since has to carry `#[serde(default)]`. That is a convention, and a
    /// convention is not a guarantee: this fixture is the guarantee. It is
    /// **frozen** — never regenerate it to make the test pass, because the
    /// whole point is that it predates the fields under test.
    ///
    /// Failing it means an upgrade silently discards the persisted baseline
    /// (`load_initial` falls back to an empty `Ir`) and re-applies everything
    /// without pruning, leaving orphaned state in Sōzu.
    #[test]
    fn a_shadow_from_an_older_controller_still_parses() {
        let raw = include_str!("../tests/fixtures/shadow-v0.2.json");
        let ir: Ir = serde_json::from_str(raw)
            .expect("an older shadow must stay readable; a new Ir field needs #[serde(default)]");
        assert_eq!(ir.clusters.len(), 1);
        assert_eq!(ir.frontends.len(), 1);
        assert_eq!(ir.backends.len(), 1);
        // Fields that did not exist when the fixture was written default
        // cleanly rather than failing the parse.
        assert!(ir.l4_frontends.is_empty());
        assert!(ir.clusters[0].max_connections_per_ip.is_none());
        assert!(ir.frontends[0].filters.redirect.is_none());
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

    fn generation_fixture(pids: [i32; 2]) -> SozuGeneration {
        SozuGeneration {
            socket: sozu_gw_agent::SocketIdentity {
                device: 1,
                inode: 10,
                changed_secs: 1,
                changed_nanos: 0,
            },
            worker_pids: std::collections::BTreeSet::from(pids),
        }
    }

    fn applied_ir() -> Ir {
        Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::default(),
                sticky_session: false,
                https_redirect: false,
                max_connections_per_ip: None,
                retry_after: None,
            }],
            ..Default::default()
        }
    }

    fn temp_shadow_path(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("sozu-gw-shadow-{name}-{}.json", std::process::id()))
            .to_str()
            .expect("utf-8 temp path")
            .to_string()
    }

    /// The whole point of persisting the generation: a Sōzu that restarted
    /// while the controller was down has a new command socket, and the file
    /// written against the old one must not be trusted.
    #[test]
    fn load_initial_ignores_a_shadow_written_against_another_socket() {
        let path = temp_shadow_path("other-socket");
        let written = generation_fixture([7, 8]);
        persist(
            &path,
            &Shadow {
                generation: Some(written.clone()),
                ir: applied_ir(),
            },
        );
        let mut restarted = written.clone();
        restarted.socket.changed_secs += 1; // same PIDs, new socket
        let loaded = load_initial(&path, Some(&restarted));
        assert_eq!(loaded.ir, Ir::default(), "a restarted Sōzu holds nothing");
        assert_eq!(loaded.generation, Some(restarted));
        let _ = std::fs::remove_file(&path);
    }

    /// A worker that bounced while the controller was down was re-fed the
    /// main process's state: same socket, other PIDs, shadow still valid.
    #[test]
    fn load_initial_resumes_on_the_same_socket_even_with_new_worker_pids() {
        let path = temp_shadow_path("same-socket");
        persist(
            &path,
            &Shadow {
                generation: Some(generation_fixture([7, 8])),
                ir: applied_ir(),
            },
        );
        let current = generation_fixture([9, 10]);
        let loaded = load_initial(&path, Some(&current));
        assert_eq!(loaded.ir, applied_ir());
        assert_eq!(
            loaded.generation,
            Some(current),
            "the resumed shadow adopts the generation that was actually observed"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_initial_without_a_generation_proof_starts_empty() {
        let path = temp_shadow_path("no-proof");
        persist(
            &path,
            &Shadow {
                generation: Some(generation_fixture([7, 8])),
                ir: applied_ir(),
            },
        );
        let loaded = load_initial(&path, None);
        assert_eq!(loaded, Shadow::empty(None));
        let _ = std::fs::remove_file(&path);
    }

    /// A file from before the envelope (a bare `Ir`, or an envelope without a
    /// generation) carries no proof and is ignored.
    #[test]
    fn load_initial_ignores_a_shadow_without_a_generation() {
        let path = temp_shadow_path("bare-ir");
        std::fs::write(&path, serde_json::to_vec(&applied_ir()).unwrap()).unwrap();
        let current = generation_fixture([7, 8]);
        let loaded = load_initial(&path, Some(&current));
        assert_eq!(loaded.ir, Ir::default());
        let _ = std::fs::remove_file(&path);
    }
}
