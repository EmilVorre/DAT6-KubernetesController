/**
 * k6 load script for drainable-service baseline experiments.
 *
 * Usage:
 *   k6 run --env TARGET_URL=http://localhost:8080 --env SCENARIO=steady load.js
 *   k6 run --env TARGET_URL=http://drainable-service --env SCENARIO=burst load.js
 *
 * Env vars:
 *   TARGET_URL  - base URL (default http://localhost:8080)
 *   SCENARIO    - steady | burst | long_requests | keepalive
 *   DURATION    - test duration (default 60s)
 *   VUS         - virtual users (default 10)
 *   RPS         - target RPS for steady (default 50)
 *   BURST_RPS   - peak RPS in burst mode (default 200)
 *   LONG_PCT    - % of long requests (2-10s) in long_requests scenario (default 5)
 */
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const errorRate = new Rate('errors');
const customLatency = new Trend('request_latency');

const TARGET_URL = __ENV.TARGET_URL || 'http://localhost:8080';
const SCENARIO = __ENV.SCENARIO || 'steady';
const DURATION = __ENV.DURATION || '60s';
const VUS = parseInt(__ENV.VUS || '10', 10);
const RPS = parseInt(__ENV.RPS || '50', 10);
const BURST_RPS = parseInt(__ENV.BURST_RPS || '200', 10);
const LONG_PCT = parseInt(__ENV.LONG_PCT || '5', 10);

export const options = {
  scenarios: buildScenarios(),
  thresholds: {
    http_req_failed: ['rate<0.1'],
    http_req_duration: ['p(99)<5000'],
  },
};

function buildScenarios() {
  const base = {
    executor: 'constant-arrival-rate',
    rate: RPS,
    timeUnit: '1s',
    duration: DURATION,
    preAllocatedVUs: VUS,
    maxVUs: VUS * 2,
  };

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
          ...base,
          rate: Math.floor(RPS * 0.5),
          exec: 'longRequests',
        },
      };
    case 'keepalive':
      return {
        keepalive: {
          ...base,
          exec: 'keepaliveHeavy',
        },
      };
    default:
      return { steady: { ...base, exec: 'steady' } };
  }
}

export function steady() {
  const res = http.get(`${TARGET_URL}/`);
  const ok = check(res, { 'status 200': (r) => r.status === 200 });
  errorRate.add(!ok);
  customLatency.add(res.timings.duration);
  sleep(1 / (RPS / VUS));
}

export function longRequests() {
  const res = http.get(`${TARGET_URL}/`);
  const ok = check(res, { 'status 200': (r) => r.status === 200 });
  errorRate.add(!ok);
  customLatency.add(res.timings.duration);
  sleep(0.5);
}

export function keepaliveHeavy() {
  const params = { headers: { Connection: 'keep-alive' } };
  const res = http.get(`${TARGET_URL}/`, params);
  const ok = check(res, { 'status 200': (r) => r.status === 200 });
  errorRate.add(!ok);
  customLatency.add(res.timings.duration);
  sleep(0.1);
}

export function burst() {
  const res = http.get(`${TARGET_URL}/`);
  const ok = check(res, { 'status 200': (r) => r.status === 200 });
  errorRate.add(!ok);
  customLatency.add(res.timings.duration);
  sleep(0.01);
}
