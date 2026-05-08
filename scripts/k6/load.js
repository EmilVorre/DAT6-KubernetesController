/**
 * k6 load script for drainable-service safe-decomposition experiments.
 *
 * Usage:
 *   k6 run --env TARGET_URL=http://localhost:8080 --env SCENARIO=steady load.js
 *   k6 run --env TARGET_URL=http://drainable-service --env SCENARIO=burst  load.js
 *
 * Env vars:
 *   TARGET_URL  - base URL (default http://localhost:8080)
 *   SCENARIO    - steady | burst | long_requests | keepalive
 *   DURATION    - test duration (default 60s)
 *   VUS         - virtual users (default 10)
 *   RPS         - target RPS for steady (default 50)
 *   BURST_RPS   - peak RPS in burst mode (default 200)
 *   LONG_PCT    - % of long requests (2-10s) in long_requests scenario (default 5)
 *   K6_EXECUTOR - "constant-arrival-rate" (default) | "per-vu-iterations".
 *
 * Test-harness caveats (read before drawing conclusions):
 *
 * 1) `constant-arrival-rate` keeps a target *rate* of starts per second; if a
 *    VU is busy waiting on a hung connection, k6 spins up additional VUs (up
 *    to `maxVUs`) to maintain the rate. That can mask true connection
 *    failures because the executor will keep trying. We therefore also expose
 *    a `per-vu-iterations` mode (set `K6_EXECUTOR=per-vu-iterations`) where
 *    every iteration counts and a failed connect is recorded as an error
 *    rather than silently retried.
 *
 * 2) `noConnectionReuse: true` is set globally so every iteration opens a
 *    fresh TCP connection to the NodePort. This is critical for shutdown
 *    experiments: with keep-alive, k6 (and Go's http transport) will happily
 *    reuse a socket pinned to a pod that has already been removed from
 *    Service endpoints — masking the very loss we're trying to measure.
 *
 * 3) `discardResponseBodies` is *off* so JSON bodies are read; combined with
 *    `tags.expected_response`, this gives us a tighter view of which
 *    responses are actually 2xx.
 */
import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

const errorRate = new Rate('errors');
const customLatency = new Trend('request_latency');
const failedRequests = new Counter('failed_requests');
const successfulRequests = new Counter('successful_requests');

const TARGET_URL = __ENV.TARGET_URL || 'http://localhost:8080';
const SCENARIO = __ENV.SCENARIO || 'steady';
const DURATION = __ENV.DURATION || '60s';
const VUS = parseInt(__ENV.VUS || '10', 10);
const RPS = parseInt(__ENV.RPS || '50', 10);
const BURST_RPS = parseInt(__ENV.BURST_RPS || '200', 10);
const LONG_PCT = parseInt(__ENV.LONG_PCT || '5', 10);
const EXECUTOR = (__ENV.K6_EXECUTOR || 'constant-arrival-rate').trim();

export const options = {
  // Force a fresh TCP connection on every request so a stale keep-alive socket
  // can't hide endpoint churn. This is critical for shutdown experiments.
  noConnectionReuse: true,
  discardResponseBodies: false,
  scenarios: buildScenarios(),
  // We deliberately do NOT set `http_req_failed: ['rate<0.1']` here because
  // some experiments are *expected* to exceed that rate (the whole point of
  // baseline). Fail thresholds are handled in post-processing.
  thresholds: {
    http_req_failed: [`rate<0.1`],
    http_req_duration: ['p(99)<5000'],
  },
  // Track non-2xx as failures explicitly rather than relying on default tags.
  tags: { test: 'drainable-service' },
};

function buildScenarios() {
  const baseConstant = {
    executor: 'constant-arrival-rate',
    rate: RPS,
    timeUnit: '1s',
    duration: DURATION,
    preAllocatedVUs: VUS,
    maxVUs: VUS * 3,
    gracefulStop: '0s',
  };
  // `per-vu-iterations` issues exactly N iterations per VU. Failed iterations
  // are recorded; nothing is silently retried by the executor. Useful when we
  // want to be sure that what we *count* matches what *happened on the wire*.
  const basePerVU = {
    executor: 'per-vu-iterations',
    vus: VUS,
    // Use RPS * (DURATION seconds) / VUS as a rough total. We compute the
    // duration suffix here defensively; if parsing fails, fall back to 60.
    iterations: Math.max(1, Math.floor((RPS * parseDurationSecs(DURATION)) / VUS)),
    maxDuration: DURATION,
  };
  const useBase = EXECUTOR === 'per-vu-iterations' ? basePerVU : baseConstant;

  switch (SCENARIO) {
    case 'burst':
      return {
        burst: {
          executor: 'ramping-arrival-rate',
          startRate: RPS,
          timeUnit: '1s',
          stages: [
            { duration: '10s', target: RPS },
            { duration: '5s', target: BURST_RPS },
            { duration: '20s', target: BURST_RPS },
            { duration: '5s', target: RPS },
            { duration: '20s', target: RPS },
          ],
          preAllocatedVUs: VUS,
          maxVUs: VUS * 4,
          exec: 'burst',
        },
      };
    case 'long_requests':
      return {
        long_requests: {
          ...useBase,
          ...(EXECUTOR === 'per-vu-iterations'
            ? {}
            : { rate: Math.floor(RPS * 0.5) }),
          exec: 'longRequests',
        },
      };
    case 'keepalive':
      return {
        keepalive: {
          ...useBase,
          exec: 'keepaliveHeavy',
        },
      };
    default:
      return { steady: { ...useBase, exec: 'steady' } };
  }
}

function parseDurationSecs(d) {
  const m = /^(\d+)([smh])?$/.exec(d.trim());
  if (!m) return 60;
  const n = parseInt(m[1], 10);
  switch (m[2]) {
    case 'm':
      return n * 60;
    case 'h':
      return n * 3600;
    default:
      return n;
  }
}

/**
 * Record one HTTP outcome. Anything other than a 200 — including transport
 * errors that k6 surfaces as `status: 0` — is counted as a failure.
 */
function recordOutcome(res) {
  const ok = check(res, { 'status 200': (r) => r.status === 200 });
  errorRate.add(!ok);
  customLatency.add(res.timings.duration);
  if (ok) {
    successfulRequests.add(1);
  } else {
    failedRequests.add(1, { status: String(res.status), error: res.error || '' });
  }
  return ok;
}

export function steady() {
  // No client-side sleep: pacing is the executor's job (constant-arrival-rate
  // schedules iterations; per-vu-iterations runs back-to-back). A `sleep` here
  // would oversubscribe the requested RPS and bias latency samples.
  recordOutcome(http.get(`${TARGET_URL}/`));
}

export function longRequests() {
  recordOutcome(http.get(`${TARGET_URL}/`));
}

/**
 * Keep-alive variant — explicitly opts back in to connection reuse for
 * comparison. Note: the global `noConnectionReuse` still applies to the other
 * scenarios; this scenario sets a per-request override.
 */
export function keepaliveHeavy() {
  const params = { headers: { Connection: 'keep-alive' } };
  recordOutcome(http.get(`${TARGET_URL}/`, params));
}

export function burst() {
  recordOutcome(http.get(`${TARGET_URL}/`));
}

export default function () {
  steady();
}
