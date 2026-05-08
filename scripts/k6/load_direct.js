/**
 * Direct pod IP load script — bypasses kube-proxy entirely.
 * Targets pod IPs directly so connection failures are immediate
 * when a pod dies, without waiting for endpoint propagation.
 */
import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

const errorRate = new Rate('errors');
const customLatency = new Trend('request_latency');
const failedRequests = new Counter('failed_requests');
const successfulRequests = new Counter('successful_requests');

// Pod IPs are passed in via env var, comma-separated
const POD_IPS = (__ENV.POD_IPS || '').split(',').filter(ip => ip.length > 0);
const DURATION = __ENV.DURATION || '150s';
const VUS = parseInt(__ENV.VUS || '100', 10);
const RPS = parseInt(__ENV.RPS || '50', 10);

if (POD_IPS.length === 0) {
  throw new Error('POD_IPS env var is required, e.g. POD_IPS=10.42.1.1,10.42.2.1,10.42.0.1');
}

export const options = {
  noConnectionReuse: true,
  discardResponseBodies: false,
  scenarios: {
    direct: {
      executor: 'constant-arrival-rate',
      rate: RPS,
      timeUnit: '1s',
      duration: DURATION,
      preAllocatedVUs: VUS,
      maxVUs: VUS * 3,
      gracefulStop: '0s',
      exec: 'direct',
    },
  },
  thresholds: {
    http_req_duration: ['p(99)<10000'],
  },
};

export function direct() {
  // Round-robin across pod IPs using VU id
  const ip = POD_IPS[(__VU - 1) % POD_IPS.length];
  const url = `http://${ip}:8080/`;
  const res = http.get(url);
  const ok = check(res, { 'status 200': (r) => r.status === 200 });
  errorRate.add(!ok);
  customLatency.add(res.timings.duration);
  if (ok) {
    successfulRequests.add(1);
  } else {
    failedRequests.add(1, { status: String(res.status), error: res.error || '' });
  }
}

export default function() {
  direct();
}