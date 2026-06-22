# PROGRESS

Journal de bord du contrôleur Ingress + Gateway API basé sur Sōzu.
Voir le prompt de cadrage pour le périmètre complet. On livre **Phase 1** (Ingress + TLS) avant tout le reste.

## Phase 1 — MVP Ingress + TLS

### Étape 1 — Vérification du protocole Sōzu ✅ (vérifiée contre un Sōzu réel)

Environnement confirmé :
- Rust `1.96.0` (stable), édition 2024 supportée. Docker, kubectl `1.36`, helm `4.2`, minikube (via devcontainer), `protoc 3.12`.
- `cargo` fonctionne (index sparse OK) ; l'API REST publique crates.io est bloquée mais sans impact.
- **`sozu-command-lib` v2.1.0** est la dernière version publiée. Deps notables : `prost 0.14`, `mio 1.2`, `nix 0.31`, `nom 7`. Licence **LGPL-3.0** (compatible plan de contrôle propriétaire).
- **Sōzu 2.1.0** : release GitHub + image `clevercloud/sozu:2.1.0` (binaire **musl** → exécuté via Docker). CLI client complet (`cluster`/`backend`/`frontend`/`listener`/`certificate`/`state`/`reload`) utilisable pour recouper la sonde. Crypto = rustls+ring.
- Cluster de test **poc-sozu-gateway-2** : propre (CNI Cilium, control-plane managé via konnectivity), aucun ingress controller.

Fait :
- Workspace Cargo scaffoldé (`ir`, `translator`, `builder`, `sozu-agent`, `controller`). `cargo check --workspace` **vert**.
- Pins de versions validés : `kube 4.0` + `k8s-openapi 0.28` (feature `v1_36`, = version du cluster).
- Source réelle de `sozu-command-lib` 2.1.0 explorée en profondeur (proto, code généré, channel/framing, state/diff, request/response, certificats).
- Cert de test auto-signé `app.example.com` généré (pour le test HTTPS de la sonde).

Fait (suite) :
- [x] `PROTOCOL.md` rédigé : types/champs/enum réellement observés (source de vérité du Translator).
- [x] Sonde `crates/sozu-agent/examples/probe.rs` + harnais `.scratch/run-probe.sh` : **HTTP 200 + HTTPS 200** à travers Sōzu, SNI OK (cert servi = le nôtre).
- [x] Ambiguïtés tranchées empiriquement : transport `Request` nu ; ack `Processing`→`Ok` (boucle obligatoire) ; listeners statiques du `config.toml` suffisent ; `ConfigState::diff` réutilisable.

Décisions en attente de validation (voir chat) :
- [ ] Translator : réutiliser `ConfigState::diff` (recommandé) vs diff maison.
- [ ] Modèle de listeners : statiques dans `config.toml` (recommandé) vs dynamiques via socket.
- [ ] Cible e2e : pas de kind/k3d ici (minikube dispo + cluster réel `poc-sozu-gateway-2`).

### Étapes 2–5 — à venir
IR + Translator (golden tests) → sozu-agent → Builder + boucle kube-rs → packaging (Dockerfile/Helm) + e2e.
