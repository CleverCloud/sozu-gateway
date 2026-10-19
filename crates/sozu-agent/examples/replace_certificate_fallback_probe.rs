//! Does a certificate rotation land on a listener Sōzu holds no certificate
//! for?
//!
//! Since Sōzu 2.3.0 (6dd569ee, sozu#1405) removing the last certificate at an
//! address drops the address from the main process's certificate map, and a
//! later `ReplaceCertificate` naming it is refused ("Did not find
//! Certificate"). That is the state a shadow lagging behind a landed
//! `RemoveCertificate` leaves (a batch that failed after it, a lost ack): the
//! next desired certificate with the same names is paired as a rotation of
//! the one Sōzu already dropped. The probe stages it with the translator's
//! own diff — add A, remove A without recording it — then sends the rotation
//! A → C raw, as Sōzu answers it, and through the agent. The main process's
//! certificate list is printed after each step; the certificate a client is
//! actually served is read separately (`openssl s_client`), since the probe
//! has no TLS client.
//!
//! Env: SOZU_SOCK (command socket), PROBE_HTTPS (an active HTTPS listener
//! holding no certificate), PROBE_NAME (the SNI name both certificates carry),
//! PROBE_CERT_A/PROBE_KEY_A and PROBE_CERT_C/PROBE_KEY_C (PEM files).

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use sozu_command_lib::certificate::{calculate_fingerprint, split_certificate_chain};
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, QueryCertificatesFilters, Request,
    Response, ResponseContent, ResponseStatus,
};
use sozu_gw_agent::SozuAgent;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// One request on a fresh connection, past the `Processing` replies: what
/// Sōzu itself answers, with none of the agent's handling.
fn send_raw(sock: &str, request: &Request) -> Result<Response> {
    let mut channel: Channel<Request, Response> =
        Channel::from_path(sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("connect {sock}"))?;
    channel.blocking().context("blocking channel")?;
    channel.write_message(request).context("write")?;
    loop {
        let response = channel.read_message().context("read")?;
        if response.status != ResponseStatus::Processing as i32 {
            return Ok(response);
        }
    }
}

/// The first 16 hex digits of a leaf's fingerprint (SHA-256 of its DER, the
/// same digest `openssl x509 -outform DER | sha256sum` prints).
fn short_fingerprint(leaf_pem: &str) -> Result<String> {
    let digest = calculate_fingerprint(leaf_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("fingerprint: {e:?}"))?;
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

/// The main process's certificates, as `fingerprint [names]`.
fn main_state(sock: &str) -> Result<String> {
    let query: Request =
        RequestType::QueryCertificatesFromTheState(QueryCertificatesFilters::default()).into();
    let response = send_raw(sock, &query)?;
    let Some(ResponseContent {
        content_type: Some(ContentType::CertificatesWithFingerprints(listed)),
    }) = response.content
    else {
        return Ok(format!("no listing: {}", response.message));
    };
    let mut out: Vec<String> = listed
        .certs
        .iter()
        .map(|(fingerprint, cert)| {
            format!(
                "{} [{}]",
                &fingerprint[..fingerprint.len().min(16)],
                cert.names.join(",")
            )
        })
        .collect();
    out.sort();
    Ok(format!("[{}]", out.join(", ")))
}

fn certificate(listener: SocketAddr, name: &str, cert: &str, key: &str) -> Result<ir::Certificate> {
    let pem = std::fs::read_to_string(cert).with_context(|| format!("read {cert}"))?;
    let mut blocks = split_certificate_chain(pem);
    if blocks.is_empty() {
        bail!("no PEM block in {cert}");
    }
    let leaf = blocks.remove(0);
    Ok(ir::Certificate {
        listener,
        certificate: leaf,
        chain: blocks,
        key: std::fs::read_to_string(key).with_context(|| format!("read {key}"))?,
        names: vec![name.to_string()],
    })
}

fn verbs(requests: &[Request]) -> String {
    requests
        .iter()
        .map(|request| match &request.request_type {
            Some(RequestType::AddCertificate(_)) => "AddCertificate",
            Some(RequestType::RemoveCertificate(_)) => "RemoveCertificate",
            Some(RequestType::ReplaceCertificate(_)) => "ReplaceCertificate",
            _ => "other",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn outcome(result: std::result::Result<(), sozu_gw_agent::SozuError>) -> String {
    match result {
        Ok(()) => "applied".to_string(),
        Err(error) => format!("FAILED: {error}"),
    }
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let https: SocketAddr = env_or("PROBE_HTTPS", "0.0.0.0:8443").parse()?;
    let name = env_or("PROBE_NAME", "app.example.com");
    let a = certificate(
        https,
        &name,
        &env_or("PROBE_CERT_A", "./cert_a.pem"),
        &env_or("PROBE_KEY_A", "./key_a.pem"),
    )?;
    let c = certificate(
        https,
        &name,
        &env_or("PROBE_CERT_C", "./cert_c.pem"),
        &env_or("PROBE_KEY_C", "./key_c.pem"),
    )?;
    let empty = ir::Ir::default();
    let with = |cert: &ir::Certificate| ir::Ir {
        certificates: vec![cert.clone()],
        ..ir::Ir::default()
    };
    let mut agent = SozuAgent::new(sock.clone());

    println!("== replace certificate fallback probe ==");
    println!(
        "A = {}, C = {}, both [{name}] at {https}",
        short_fingerprint(&a.certificate)?,
        short_fingerprint(&c.certificate)?
    );
    println!("before: {}", main_state(&sock)?);

    let add = tr::reconcile(&empty, &with(&a))?;
    println!("add A ({}): {}", verbs(&add), outcome(agent.apply(&add)));
    println!("  main: {}", main_state(&sock)?);

    // Lands in Sōzu, never reaches the shadow, which keeps holding A.
    let remove = tr::reconcile(&with(&a), &empty)?;
    println!(
        "remove A, unrecorded ({}): {}",
        verbs(&remove),
        outcome(agent.apply(&remove))
    );
    println!("  main: {}", main_state(&sock)?);

    let rotation = tr::reconcile(&with(&a), &with(&c))?;
    let [replace] = rotation.as_slice() else {
        bail!("expected one request for A -> C, got {}", verbs(&rotation));
    };
    let raw = send_raw(&sock, replace)?;
    let status = if raw.status == ResponseStatus::Ok as i32 {
        "OK"
    } else {
        "FAILURE"
    };
    println!(
        "rotate A -> C ({}), raw: {status} | {}",
        verbs(&rotation),
        raw.message
    );
    println!("  main: {}", main_state(&sock)?);
    if raw.status == ResponseStatus::Ok as i32 {
        // Restage the empty listener so the agent faces the same state.
        let undo = tr::reconcile(&with(&c), &empty)?;
        println!(
            "remove C to restage ({}): {}",
            verbs(&undo),
            outcome(agent.apply(&undo))
        );
        println!("  main: {}", main_state(&sock)?);
    }

    let first = agent.apply(&rotation);
    let failed = first.is_err();
    println!(
        "rotate A -> C ({}), through the agent: {}",
        verbs(&rotation),
        outcome(first)
    );
    println!("  main: {}", main_state(&sock)?);
    if failed {
        // The shadow did not advance, so the next reconcile sends it again.
        println!(
            "rotate A -> C again, the next reconcile: {}",
            outcome(agent.apply(&rotation))
        );
        println!("  main: {}", main_state(&sock)?);
    }
    Ok(())
}
