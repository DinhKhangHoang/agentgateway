//! FR-5.1-5.3 active health prober.
//!
//! Periodically sends a tiny probe request to a sampled endpoint of an AI
//! backend and records the outcome through the same `ActiveHandle::finish_request`
//! path as real traffic — so a dead backend is evicted *before* the next real
//! request hits it (proactive, not reactive).
//!
//! Modeled on the eviction worker (`types::loadbalancer::EndpointSet::worker`):
//! a `tokio::task::spawn` loop that sleeps for the configured interval, probes
//! one P2C-sampled endpoint per sweep, and writes health via the captured
//! `ActiveHandle`. Kill-switch is a generation counter: the store bumps it on
//! `insert_backend`/`remove_backend`, and the prober exits when its captured
//! generation is stale — no `Arc<AtomicU64>` drift, no leaked tasks across
//! reloads.
//!
//! Scope: one sampled endpoint per sweep (not every endpoint). A dead backend
//! is evicted when the sampler picks it; worst-case detection is ~N sweeps at
//! `interval` each. This reuses `make_backend_call`'s dispatch tail
//! (`build_transport` + `apply_backend_auth` + `setup_request` + `upstream.call`)
//! without duplicating the full request-translation pipeline.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::http::header;
use ::http::{Method, Request};
use agent_core::prelude::Strng;
use tokio::time::sleep;
use tracing::{debug, trace};

use crate::http::health::{self, ActiveProbeConfig};
use crate::llm::{AIBackend, NamedAIProvider, RouteType};
use crate::proxy::httpproxy::{build_transport, BackendCall};
use crate::types::agent::{BackendTarget, BackendTargetRef, Target};
use crate::types::loadbalancer::ActiveHandle;
use crate::ProxyInputs;

/// Per-backend generation counters, shared between the store (which bumps them
/// on backend insert/remove) and prober tasks (which exit when their captured
/// generation goes stale). Keyed by `BackendKey` (= `Strng`).
///
/// Lives on the bind store so a prober spawned on one reload is reliably killed
/// by the next reload that touches the same backend key.
#[derive(Default)]
pub struct ProberGenerationRegistry {
    generations: Mutex<std::collections::HashMap<Strng, Arc<AtomicU64>>>,
    /// Keys with a currently-running prober. Dedup so concurrent first-requests
    /// to the same backend don't double-spawn. Cleared by the prober on exit.
    in_flight: Mutex<std::collections::HashSet<Strng>>,
}

impl std::fmt::Debug for ProberGenerationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProberGenerationRegistry")
            .finish_non_exhaustive()
    }
}

/// Clears the in-flight marker for a backend key when dropped, so a prober
/// exiting for any reason (generation stale, backend dropped, panic) lets a
/// future reload re-claim the spawn slot.
struct InFlightGuard {
    key: Strng,
    registry: Arc<ProberGenerationRegistry>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.registry.clear_in_flight(&self.key);
    }
}

impl ProberGenerationRegistry {
    /// Bump the generation for `key`, invalidating any running prober. Called
    /// from `insert_backend`/`remove_backend` so a prober spawned on a prior
    /// reload exits before a new one starts.
    pub fn bump(&self, key: &Strng) {
        let gens = self.generations.lock().expect("prober generation mutex poisoned");
        if let Some(generation) = gens.get(key) {
            generation.fetch_add(1, Ordering::SeqCst);
        }
        // If absent, there was no prober to kill; nothing to bump.
    }

    /// Atomically claim the right to spawn a prober for `key`. Returns the
    /// generation `Arc` and the value to capture if no prober is currently
    /// running for `key`; `None` if one is. The caller must `mark_in_flight`
    /// before spawning and the task must `clear_in_flight` on exit.
    pub fn try_claim_spawn(&self, key: &Strng) -> Option<(Arc<AtomicU64>, u64)> {
        let mut gens = self.generations.lock().expect("prober generation mutex poisoned");
        let in_flight = self.in_flight.lock().expect("prober in_flight mutex poisoned");
        if in_flight.contains(key) {
            return None;
        }
        let entry = gens
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
        Some((entry.clone(), entry.load(Ordering::SeqCst)))
    }

    /// Mark a prober for `key` as in-flight (running). Called right after
    /// `try_claim_spawn` succeeds, before the task is spawned.
    pub fn mark_in_flight(&self, key: &Strng) {
        let mut in_flight = self.in_flight.lock().expect("prober in_flight mutex poisoned");
        in_flight.insert(key.clone());
    }

    /// Clear the in-flight marker for `key` (the prober exited). Called from
    /// the prober task on exit so a future `try_claim_spawn` can re-claim.
    pub fn clear_in_flight(&self, key: &Strng) {
        let mut in_flight = self.in_flight.lock().expect("prober in_flight mutex poisoned");
        in_flight.remove(key);
    }
}

/// Probe a single sampled endpoint of an AI backend on an interval, feeding
/// the outcome into `ActiveHandle::finish_request` (the same health/eviction
/// path as real traffic). Exits when either (a) `generation` no longer matches
/// the registry's current value for the backend key — bumped by the store on
/// `insert_backend`/`remove_backend` — or (b) the `Weak` upgrade fails, meaning
/// the store has already dropped the backend's `Arc`. Belt-and-suspenders: the
/// generation bump fires before the `Arc` drops, so (a) fires first; (b) covers
/// any window where the bump raced with a concurrent sweep.
///
/// The prober holds a `Weak<BackendWithPolicies>` (not a clone of the
/// `AIBackend`) so each sweep probes the *live* `EndpointSet` that real traffic
/// mutates — health writes via `finish_request` propagate to real-request
/// selection immediately. A cloned `EndpointSet` would record health on a
/// snapshot invisible to real traffic.
pub fn spawn(
    inputs: Arc<ProxyInputs>,
    backend_key: Strng,
    backend: std::sync::Weak<crate::types::agent::BackendWithPolicies>,
    policy: health::Policy,
    probe_cfg: ActiveProbeConfig,
    generation: Arc<AtomicU64>,
    spawned_generation: u64,
    registry: Arc<ProberGenerationRegistry>,
) {
    let interval = probe_cfg.interval_or_default();
    let timeout = probe_cfg.timeout_or_default();
    let consecutive_threshold = probe_cfg.consecutive_failures_or_default() as u64;
    let inputs_for_loop = inputs.clone();
    let key_for_guard = backend_key.clone();
    let registry_for_guard = registry.clone();

    tokio::task::spawn(async move {
        // Clear the in-flight marker on exit (including panic-unwind) so a
        // future reload can re-claim. The guard captures the key + registry.
        let _guard = InFlightGuard {
            key: key_for_guard,
            registry: registry_for_guard,
        };
        let mut probe_consecutive_failures: u64 = 0;
        debug!(
            %backend_key,
            ?interval,
            ?timeout,
            "health prober started (backend_key={})", backend_key
        );
        loop {
            // Sleep first: a just-spawned prober lets real traffic establish health
            // before the first probe. Mirrors the eviction worker's `maybe_sleep_until`.
            sleep(interval).await;

            // Kill-switch (primary): the store bumped the generation on
            // reload/replace. This fires before the Weak goes dead.
            if generation.load(Ordering::SeqCst) != spawned_generation {
                debug!(%backend_key, "health prober generation stale, exiting");
                return;
            }

            // Kill-switch (secondary): the Weak upgrades to the live backend.
            // `None` means the store dropped the Arc (reload replaced it); the
            // generation check above usually catches this first, but this covers
            // the race window and lets us dereference the live EndpointSet.
            let Some(live) = backend.upgrade() else {
                debug!(%backend_key, "health prober backend dropped, exiting");
                return;
            };
            let crate::types::agent::Backend::AI(_, ai) = &live.backend else {
                // Backendkind changed on reload (e.g. AI → Service); exit —
                // a new prober (if any) spawns from the new path.
                debug!(%backend_key, "health prober backend no longer AI, exiting");
                return;
            };

            if let Err(e) = probe_once(
                &inputs_for_loop,
                ai,
                &policy,
                timeout,
                consecutive_threshold,
                &mut probe_consecutive_failures,
            )
            .await
            {
                trace!(%backend_key, "health probe sweep error: {e}");
                // A sweep that failed to dispatch at all (e.g. no healthy
                // endpoint to sample) is not itself an endpoint-health signal;
                // do not record a failure against an unknown endpoint.
            }
        }
    });
}

/// One probe sweep: sample one endpoint, dispatch a tiny request, record the
/// outcome on the captured `ActiveHandle`.
async fn probe_once(
    inputs: &Arc<ProxyInputs>,
    ai: &AIBackend,
    policy: &health::Policy,
    timeout: Duration,
    consecutive_threshold: u64,
    probe_consecutive_failures: &mut u64,
) -> anyhow::Result<()> {
    // Select ONE endpoint via the same P2C sampler as real traffic. The
    // returned `ActiveHandle` is the exact endpoint we must probe and record.
    let Some((provider, handle)) = ai.select_provider() else {
        // No endpoint available (all evicted). Nothing to probe; the probe's
        // own failure count is irrelevant here — a real request would already
        // 503. Leave health as-is.
        return Ok(());
    };

    let started = Instant::now();
    let outcome = dispatch_probe(inputs, &provider, timeout).await;
    let latency = started.elapsed();

    let (success, status) = match outcome {
        Ok(status) => (status.is_success(), Some(status)),
        Err(_) => (false, None),
    };

    record_outcome(
        handle,
        policy,
        success,
        latency,
        status,
        consecutive_threshold,
        probe_consecutive_failures,
    );
    Ok(())
}

/// Build and dispatch a minimal probe request to the provider's target. Reuses
/// `build_transport`, `apply_backend_auth`, and `NamedAIProvider::setup_request`
/// — the same primitives as `make_backend_call`'s dispatch tail — so TLS, auth,
/// path/host, and per-provider headers (e.g. `anthropic-version`) are identical
/// to real traffic, but the request-translation pipeline is not invoked (the
/// probe body is already in the chat-completions dialect).
async fn dispatch_probe(
    inputs: &Arc<ProxyInputs>,
    provider: &NamedAIProvider,
    timeout: Duration,
) -> Result<::http::StatusCode, anyhow::Error> {
    let route_type = RouteType::Completions;
    let target = match &provider.host_override {
        Some(t) => t.clone(),
        None => provider
            .provider
            .default_connector_target(route_type)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "custom provider {} without host_override is not probeable",
                    provider.name
                )
            })?,
    };

    // Build the probe body: chat-completions dialect, 1 token, no tools. This is
    // accepted cross-provider on /v1/chat/completions. Do NOT use
    // `max_completion_tokens` — Anthropic rejects it on /v1/messages even though
    // this path targets /v1/chat/completions; `max_tokens` is universally safe.
    // `model` is the provider's own name (the gateway rewrites client model
    // names to backend names; a probe must use the backend name).
    let body = serde_json::json!({
        "model": provider.name.as_str(),
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1i32,
        "stream": false,
    })
    .to_string();

    let mut req = Request::builder()
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .body(crate::http::Body::from(body))
        .map_err(|e| anyhow::anyhow!("build probe request: {e}"))?;

    // Provider path/host/required-headers (e.g. anthropic-version). `host_override`
    // providers skip default authority; `path_override` providers pin the path.
    provider
        .provider
        .setup_request(
            &mut req,
            route_type,
            None,
            provider.path_override.as_deref(),
            provider.path_prefix.as_deref(),
            provider.host_override.is_some(),
        )
        .map_err(|e| anyhow::anyhow!("probe setup_request: {e}"))?;

    // Resolve effective policies: provider defaults (TLS + provider auth kind)
    // merged with the backend's sub-backend policies (which carry the API key).
    let provider_defaults = provider
        .provider
        .default_connector_policies()
        .unwrap_or_default();
    // The sub-backend reference must match how the backend was registered so the
    // store resolves the same policies (and the API key) that real traffic gets.
    // Keyed on the provider name (section) within the AI backend (name).
    let sub_backend_ref = BackendTargetRef::Backend {
        name: provider.name.as_ref(),
        namespace: "",
        section: Some(provider.name.as_ref()),
    };
    let sub_policies = inputs
        .stores
        .read_binds()
        .sub_backend_policies(sub_backend_ref, Some(&provider.inline_policies));
    let effective = provider_defaults.merge(sub_policies);

    // Apply the API key / provider auth. For Anthropic this relocates
    // `Authorization: Bearer` → `x-api-key` (set_required_fields above already
    // ran, so apply_backend_auth runs after; order matches make_backend_call).
    let backend_call = BackendCall::from_shared(target.clone(), Arc::new(effective.clone()));
    let backend_info = crate::http::auth::BackendInfo {
        target: BackendTarget::Backend {
            name: provider.name.clone(),
            namespace: Strng::default(),
            section: Some(provider.name.clone()),
        },
        call_target: target.clone(),
        inputs: inputs.clone(),
    };
    if let Some(auth) = effective.backend_auth.as_ref() {
        crate::http::auth::apply_backend_auth(&backend_info, auth, &mut req)
            .await
            .map_err(|e| anyhow::anyhow!("probe apply_backend_auth: {e}"))?;
    }

    // Set Host from the target (apply_auto_hostname equivalent).
    set_host(&mut req, &target)?;

    let transport = build_transport(
        inputs,
        &backend_call,
        None, // hbone_source: probes originate from the gateway, not a waypoint
        effective.backend_tls.clone(),
        effective.tunnel.as_ref(),
        effective
            .http
            .as_ref()
            .and_then(|h| h.version)
            .or(backend_call.http_version_override),
    )
    .await
    .map_err(|e| anyhow::anyhow!("probe build_transport: {e}"))?;

    let call = crate::client::Call {
        req,
        target,
        transport,
    };

    let resp = match tokio::time::timeout(timeout, inputs.upstream.call(call)).await {
        Ok(r) => r.map_err(|e| anyhow::anyhow!("probe upstream call: {e}"))?,
        Err(_) => return Err(anyhow::anyhow!("probe timed out after {timeout:?}")),
    };
    Ok(resp.status())
}

/// Record a probe outcome on the captured `ActiveHandle`, mirroring
/// `RequestLog::finish_request_handle_with_attempt` (telemetry/log.rs:974) but
/// without the log/CEL machinery — the probe has no request log, so we drive
/// `eviction_decision` + `finish_request` directly.
fn record_outcome(
    handle: ActiveHandle,
    policy: &health::Policy,
    success: bool,
    latency: Duration,
    status: Option<::http::StatusCode>,
    consecutive_threshold: u64,
    probe_consecutive_failures: &mut u64,
) {
    // `probe_consecutive_failures` tracks PROBE failures specifically
    // (independent of real-request consecutive_failures). The eviction decision
    // uses the handle's real-request consecutive_failures as the base, plus the
    // probe's own streak gated by `consecutive_threshold`.
    if success {
        *probe_consecutive_failures = 0;
    } else {
        *probe_consecutive_failures += 1;
    }

    // Only let a probe *evict* once its own consecutive threshold is met — a
    // single probe blip must not evict (a real request might succeed right
    // after). Below threshold, the probe still records health (0.0) so the EWMA
    // degrades, but does not trigger eviction.
    let probe_unhealthy = !success && *probe_consecutive_failures >= consecutive_threshold.max(1);
    let current_health = handle.health_score();
    let current_consecutive = handle.consecutive_failures();
    let times_ejected = handle.times_ejected();

    let (is_healthy, eviction_duration, restore_health) = policy.eviction_decision(
        current_health,
        current_consecutive,
        times_ejected,
        probe_unhealthy,
        None, // fallback_duration: probe-driven eviction uses configured duration only
    );

    handle.finish_request(
        is_healthy,
        latency,
        eviction_duration,
        restore_health,
    );

    if !success {
        debug!(
            status = ?status,
            success,
            probe_consecutive_failures,
            probe_unhealthy,
            evicted = eviction_duration.is_some(),
            "health probe recorded failure"
        );
    } else {
        trace!(success, "health probe recorded success");
    }
}

/// Set the Host header and URI authority from the target — the
/// `apply_auto_hostname` equivalent for the probe path. The probe request does
/// not carry the `AutoHostname` extension (set by the request-processing
/// pipeline for real traffic), so we set the authority directly. The scheme is
/// left to `build_transport` (TLS→https, plaintext→http).
fn set_host(req: &mut Request<crate::http::Body>, target: &Target) -> anyhow::Result<()> {
    let hostport = target.hostport();
    // Extract the host (no port) for the URI authority when the target is a
    // hostname; for a socket address the hostport is fine as the authority.
    let authority_str = match target {
        Target::Hostname(host, port) => {
            if *port == 443 || *port == 80 {
                host.as_str().to_string()
            } else {
                hostport.clone()
            }
        },
        _ => hostport.clone(),
    };
    crate::http::modify_req_uri(req, |uri| {
        uri.authority = Some(::http::uri::Authority::from_str(&authority_str)?);
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("set probe host: {e}"))?;
    if !req.headers().contains_key(header::HOST) {
        req.headers_mut()
            .insert(header::HOST, header::HeaderValue::from_str(&hostport)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn key(s: &str) -> Strng {
		Strng::from(s.to_string())
	}

	#[test]
	fn try_claim_spawn_dedups_concurrent_callers() {
		let registry = ProberGenerationRegistry::default();
		let k = key("backend-a");
		// First claim succeeds.
		let first = registry.try_claim_spawn(&k);
		assert!(first.is_some(), "first claim should succeed");
		registry.mark_in_flight(&k);
		// Second claim while in-flight is rejected.
		let second = registry.try_claim_spawn(&k);
		assert!(second.is_none(), "second claim while in-flight should be rejected");
		// After the prober exits (clears in-flight), a new claim succeeds with
		// the SAME generation Arc but possibly a bumped value.
		registry.clear_in_flight(&k);
		let third = registry.try_claim_spawn(&k);
		assert!(third.is_some(), "claim after clear should succeed");
	}

	#[test]
	fn bump_invalidates_running_prober() {
		let registry = ProberGenerationRegistry::default();
		let k = key("backend-b");
		let (generation, spawned_generation) =
			registry.try_claim_spawn(&k).expect("claim succeeds");
		// Captured generation matches what the prober would check.
		assert_eq!(generation.load(Ordering::SeqCst), spawned_generation);
		// A reload bumps the generation.
		registry.bump(&k);
		let now = generation.load(Ordering::SeqCst);
		assert_ne!(now, spawned_generation, "bump must diverge from captured value");
		// The prober's kill-switch check fires: generation != spawned_generation.
		assert!(now != spawned_generation, "prober should exit");
	}

	#[test]
	fn bump_on_absent_key_is_a_noop() {
		let registry = ProberGenerationRegistry::default();
		// No prober registered for this key; bump must not panic.
		registry.bump(&key("never-spawned"));
	}

	#[test]
	fn independent_keys_do_not_interfere() {
		let registry = ProberGenerationRegistry::default();
		let ka = key("backend-a");
		let kb = key("backend-b");
		let (gen_a, spawn_a) = registry.try_claim_spawn(&ka).expect("claim a");
		registry.mark_in_flight(&ka);
		// Bumping backend-b must not affect backend-a's prober.
		registry.bump(&kb);
		assert_eq!(
			gen_a.load(Ordering::SeqCst),
			spawn_a,
			"bumping a different key must not invalidate this prober"
		);
	}

	#[test]
	fn re_claim_after_bump_uses_bumped_generation() {
		let registry = ProberGenerationRegistry::default();
		let k = key("backend-c");
		let (generation, spawn_gen) = registry.try_claim_spawn(&k).expect("first claim");
		registry.mark_in_flight(&k);
		registry.clear_in_flight(&k);
		// Simulate a reload (store bumps on insert).
		registry.bump(&k);
		let (gen2, spawn_gen2) = registry.try_claim_spawn(&k).expect("re-claim after bump");
		// Same Arc (the entry persists), but the captured value is now bumped.
		assert!(Arc::ptr_eq(&generation, &gen2), "generation Arc is reused across claims");
		assert_eq!(
			spawn_gen2,
			spawn_gen + 1,
			"re-claim captures the bumped generation value"
		);
	}
}
