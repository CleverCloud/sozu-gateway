//! Which certificate does Sōzu serve for an SNI name two certificates claim,
//! and does the builder's fix — strip the name from the newer claimant's
//! certificate — take effect on a running Sōzu, without a gap?
//!
//! Two certificates at one HTTPS listener: the victim's (`cert_a`, expires
//! 2036-06-19) and the attacker's (`cert_b`, expires 2036-09-18). Every batch
//! is what the translator emits between two IRs, applied through the agent;
//! the outcome is read from fresh TLS handshakes, never from the agent's
//! verdict. A name served by neither certificate prints as `default` (Sōzu's
//! built-in certificate, i.e. the name lost its own).
//!
//! The last runs race handshakes against a name changing hands in one batch,
//! many times over: in the translator's order, then with one batch reversed
//! at a time, then both. Handing the name to the victim (`give`) is the order
//! `CertRank` fixes — the winner's certificate gains the name before the
//! loser's drops it. Handing it back (`take`) is the attacker's certificate
//! regaining it before the victim's is removed, which the tiers already
//! ordered. Each inversion is measured on its own so neither is credited
//! with the other's fallbacks.
//!
//! Env: SOZU_SOCK (command socket), PROBE_HTTPS (the HTTPS listener address
//! Sōzu holds), PROBE_CONNECT (where that listener is reachable from here),
//! PROBE_ROUNDS (round trips per race, default 40).

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use sozu_command_lib::certificate::{calculate_fingerprint, calculate_fingerprint_from_der};
use sozu_command_lib::proto::command::{request::RequestType, Request};
use sozu_gw_agent::SozuAgent;
use sozu_gw_ir as ir;
use sozu_gw_translator::reconcile;

const VICTIM_CERT: &str = include_str!("../../builder/tests/fixtures/cert_a.pem");
const VICTIM_KEY: &str = include_str!("../../builder/tests/fixtures/key_a.pem");
const ATTACKER_CERT: &str = include_str!("../../builder/tests/fixtures/cert_b.pem");
const ATTACKER_KEY: &str = include_str!("../../builder/tests/fixtures/key_b.pem");
const CONTESTED: &str = "api.probe";
const ATTACKER_OWN: &str = "attacker.probe";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Accepts any server certificate: the probe asks which one was served, not
/// whether it is trusted. The handshake signatures are still checked.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

struct Prober {
    connect: SocketAddr,
    config: Arc<ClientConfig>,
    victim: Vec<u8>,
    attacker: Vec<u8>,
}

impl Prober {
    /// Which certificate one fresh handshake for `sni` is served.
    fn served(&self, sni: &str) -> String {
        match self.leaf(sni) {
            Ok(der) => {
                let fingerprint = calculate_fingerprint_from_der(&der);
                if fingerprint == self.victim {
                    "victim".to_string()
                } else if fingerprint == self.attacker {
                    "attacker".to_string()
                } else {
                    let short: String = fingerprint
                        .iter()
                        .take(4)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    format!("default({short})")
                }
            }
            Err(error) => format!("failed({error})"),
        }
    }

    fn leaf(&self, sni: &str) -> Result<Vec<u8>> {
        let name = ServerName::try_from(sni.to_string())?;
        let mut tls = ClientConnection::new(self.config.clone(), name)?;
        let mut socket = TcpStream::connect_timeout(&self.connect, Duration::from_secs(5))?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        while tls.is_handshaking() {
            tls.complete_io(&mut socket)?;
        }
        let leaf = tls
            .peer_certificates()
            .and_then(|chain| chain.first())
            .ok_or_else(|| anyhow!("no certificate"))?;
        Ok(leaf.to_vec())
    }

    /// `n` fresh handshakes for `sni`, as `label: count` per certificate.
    fn tally(&self, sni: &str, n: usize) -> String {
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for _ in 0..n {
            *counts.entry(self.served(sni)).or_default() += 1;
        }
        counts
            .iter()
            .map(|(label, count)| format!("{label}: {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn certificate(listener: SocketAddr, pem: &str, key: &str, names: &[&str]) -> ir::Certificate {
    ir::Certificate {
        listener,
        certificate: pem.to_string(),
        chain: vec![],
        key: key.to_string(),
        names: names.iter().map(|n| n.to_string()).collect(),
    }
}

fn certificates(certificates: Vec<ir::Certificate>) -> ir::Ir {
    ir::Ir {
        certificates,
        ..ir::Ir::default()
    }
}

/// One line per request: verb, whose certificate, the names it installs.
fn describe(prober: &Prober, batch: &[Request]) -> String {
    let whose = |pem: &str| {
        let fingerprint = calculate_fingerprint(pem.as_bytes()).unwrap_or_default();
        if fingerprint == prober.victim {
            "victim"
        } else if fingerprint == prober.attacker {
            "attacker"
        } else {
            "?"
        }
    };
    batch
        .iter()
        .map(|request| match &request.request_type {
            Some(RequestType::AddCertificate(add)) => format!(
                "AddCertificate({}, {:?})",
                whose(&add.certificate.certificate),
                add.certificate.names
            ),
            Some(RequestType::ReplaceCertificate(replace)) => format!(
                "ReplaceCertificate({} -> {}, {:?})",
                if replace.old_fingerprint
                    == hex(
                        &calculate_fingerprint(replace.new_certificate.certificate.as_bytes())
                            .unwrap_or_default()
                    )
                {
                    "same fingerprint"
                } else {
                    "rotation"
                },
                whose(&replace.new_certificate.certificate),
                replace.new_certificate.names
            ),
            Some(RequestType::RemoveCertificate(remove)) => {
                format!("RemoveCertificate({})", &remove.fingerprint[..8])
            }
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn apply(agent: &mut SozuAgent, batch: &[Request]) -> String {
    match agent.apply(batch) {
        Ok(()) => "applied".to_string(),
        Err(error) => format!("FAILED: {error}"),
    }
}

/// Which batches of a [`race`] are applied back to front.
#[derive(Clone, Copy)]
enum Reversed {
    Neither,
    Give,
    Take,
    Both,
}

/// Hand the contested name from the attacker to the victim (`give`) and back
/// (`take`), `rounds` times, while another thread handshakes for it without
/// pause.
fn race(
    agent: &mut SozuAgent,
    prober: &Arc<Prober>,
    attacker_only: &ir::Ir,
    arbitrated: &ir::Ir,
    rounds: usize,
    reversed: Reversed,
) -> Result<String> {
    let stop = Arc::new(AtomicBool::new(false));
    let handshakes = {
        let (stop, prober) = (stop.clone(), prober.clone());
        std::thread::spawn(move || {
            let mut counts = std::collections::BTreeMap::<String, usize>::new();
            while !stop.load(Ordering::Relaxed) {
                *counts.entry(prober.served(CONTESTED)).or_default() += 1;
            }
            counts
        })
    };
    let mut give = reconcile(attacker_only, arbitrated)?;
    let mut take = reconcile(arbitrated, attacker_only)?;
    if matches!(reversed, Reversed::Give | Reversed::Both) {
        give.reverse();
    }
    if matches!(reversed, Reversed::Take | Reversed::Both) {
        take.reverse();
    }
    let mut failures = 0;
    for _ in 0..rounds {
        for batch in [&give, &take] {
            if agent.apply(batch).is_err() {
                failures += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let counts = handshakes
        .join()
        .map_err(|_| anyhow!("handshake thread panicked"))?;
    Ok(format!(
        "give: {}; take: {}; {} batches failed; {CONTESTED} served: {}",
        describe(prober, &give),
        describe(prober, &take),
        failures,
        counts
            .iter()
            .map(|(label, count)| format!("{label}: {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let https: SocketAddr = env_or("PROBE_HTTPS", "0.0.0.0:8443").parse()?;
    let connect: SocketAddr = env_or("PROBE_CONNECT", "127.0.0.1:8443").parse()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    let prober = Arc::new(Prober {
        connect,
        config: Arc::new(config),
        victim: calculate_fingerprint(VICTIM_CERT.as_bytes()).context("victim fingerprint")?,
        attacker: calculate_fingerprint(ATTACKER_CERT.as_bytes())
            .context("attacker fingerprint")?,
    });
    let mut agent = SozuAgent::new(sock);

    let victim = |names: &[&str]| certificate(https, VICTIM_CERT, VICTIM_KEY, names);
    let attacker = |names: &[&str]| certificate(https, ATTACKER_CERT, ATTACKER_KEY, names);
    // What the builder emitted before name ownership: both claims as given.
    let unarbitrated = certificates(vec![
        victim(&[CONTESTED]),
        attacker(&[CONTESTED, ATTACKER_OWN]),
    ]);
    // What it emits now that the victim is the older claimant.
    let arbitrated = certificates(vec![victim(&[CONTESTED]), attacker(&[ATTACKER_OWN])]);
    // The victim's object deleted: the attacker is the only claimant left.
    let attacker_only = certificates(vec![attacker(&[CONTESTED, ATTACKER_OWN])]);

    println!("== sni ownership probe ==");
    println!(
        "victim {} (notAfter 2036-06-19), attacker {} (notAfter 2036-09-18)",
        &hex(&prober.victim)[..8],
        &hex(&prober.attacker)[..8]
    );

    let steps: [(&str, &ir::Ir, &ir::Ir); 4] = [
        ("both claim the name", &ir::Ir::default(), &unarbitrated),
        ("strip it from the attacker", &unarbitrated, &arbitrated),
        ("victim deleted", &arbitrated, &attacker_only),
        ("victim back, older", &attacker_only, &arbitrated),
    ];
    for (step, from, to) in steps {
        let batch = reconcile(from, to)?;
        println!("-- {step}: {}", describe(&prober, &batch));
        println!("   {}", apply(&mut agent, &batch));
        println!(
            "   {CONTESTED}: {} | {ATTACKER_OWN}: {}",
            prober.tally(CONTESTED, 6),
            prober.tally(ATTACKER_OWN, 6)
        );
    }

    // The state between the two requests if the loser's went first.
    let batch = reconcile(&arbitrated, &attacker_only)?;
    println!("-- back to the attacker: {}", apply(&mut agent, &batch));
    let strip_first: Vec<Request> = reconcile(&attacker_only, &arbitrated)?
        .into_iter()
        .filter(|r| matches!(r.request_type, Some(RequestType::ReplaceCertificate(_))))
        .collect();
    println!(
        "-- only the loser's request: {}",
        describe(&prober, &strip_first)
    );
    println!("   {}", apply(&mut agent, &strip_first));
    println!("   {CONTESTED}: {}", prober.tally(CONTESTED, 6));
    let add: Vec<Request> = reconcile(&attacker_only, &arbitrated)?
        .into_iter()
        .filter(|r| matches!(r.request_type, Some(RequestType::AddCertificate(_))))
        .collect();
    println!("-- then the winner's: {}", apply(&mut agent, &add));
    println!("   {CONTESTED}: {}", prober.tally(CONTESTED, 6));

    let batch = reconcile(&arbitrated, &attacker_only)?;
    println!("-- back to the attacker: {}", apply(&mut agent, &batch));
    let rounds: usize = env_or("PROBE_ROUNDS", "40").parse()?;
    for (label, reversed) in [
        ("translator order", Reversed::Neither),
        ("give reversed", Reversed::Give),
        ("take reversed", Reversed::Take),
        ("both reversed", Reversed::Both),
    ] {
        println!(
            "-- race, {label}, {rounds} rounds: {}",
            race(
                &mut agent,
                &prober,
                &attacker_only,
                &arbitrated,
                rounds,
                reversed
            )?
        );
    }
    Ok(())
}
