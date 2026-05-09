# DAT6-KubernetesController — experiment runner
#
# All real logic lives in scripts/. This Makefile is just thin entry points.
#
# Common workflows
# ----------------
# Smoke test (1 repeat per combo, ~20 min):
#   make run-all-auto N=1
#
# Full matrix (5 repeats per combo, ~90 min):
#   make run-all-auto N=5
#
# Single combo:
#   make run-combo STRAT=baseline SCENARIO=rollout N=3
#
# Kill any in-flight experiment (handles duplicate processes too):
#   make stop
#
# Sanity-check what's deployed right now:
#   make verify-env
#
# Profile / load overrides
# ------------------------
# EXP_PROFILE=thesis-stress      RPS=250, VUS=250, DURATION=150s   (default)
# EXP_PROFILE=standard           RPS=50,  VUS=50,  DURATION=90s
# K6_RPS=400 K6_VUS=400          override individual knobs

APP_NAMESPACE   ?= default
N               ?= 5
STRAT           ?= baseline
SCENARIO        ?= rollout
EXP_PROFILE     ?= thesis-stress
CONTROLLER_IMG  ?= ghcr.io/emilvorre/dat6-controller:latest

.PHONY: help build-app push-app build-controller push-controller \
        deploy undeploy deploy-controller undeploy-controller \
        verify-controller run-all-auto run-combo stop verify-env clean

help:
	@echo "DAT6 experiment runner"
	@echo ""
	@echo "Run experiments:"
	@echo "  make run-all-auto N=1            Smoke test"
	@echo "  make run-all-auto N=5            Full matrix"
	@echo "  make run-combo STRAT=s1-early-readiness SCENARIO=rollout N=3"
	@echo ""
	@echo "Process control:"
	@echo "  make stop                        Kill any running experiment"
	@echo "  make verify-env                  Show DAT6_* env on a running pod"
	@echo ""
	@echo "Image build:"
	@echo "  make build-app                   docker build app"
	@echo "  make push-app                    docker push app to GHCR"
	@echo "  make build-controller            docker build controller"
	@echo "  make push-controller             docker push controller, rollout-restart, verify imageID"
	@echo ""
	@echo "Manual deploy:"
	@echo "  make deploy STRAT=baseline           Apply app overlay"
	@echo "  make undeploy                        Delete app deployment"
	@echo "  make deploy-controller STRAT=...     Apply controller overlay"
	@echo "  make undeploy-controller             Delete controller deployment"
	@echo "  make verify-controller               Show running controller image + env"
	@echo ""
	@echo "Profile (env var; override on command line):"
	@echo "  EXP_PROFILE=standard make run-all-auto N=1"
	@echo "  K6_RPS=400 K6_VUS=400 make run-all-auto N=3"

# --- image: app ---
build-app:
	docker build -t ghcr.io/emilvorre/drainable-service:latest -f app/Dockerfile app/

push-app: build-app
	docker push ghcr.io/emilvorre/drainable-service:latest

# --- image: controller ---
# Mirrors push-app: build, push, then force a rollout and confirm the new
# imageID actually landed on the running pod. The verify step matters because
# imagePullPolicy: Always + :latest can still serve a stale layer if the
# kubelet's image cache silently hits — we got bitten by exactly this on the
# app, so we don't trust the rollout until imageID changes.
build-controller:
	DOCKER_BUILDKIT=1 docker build -t $(CONTROLLER_IMG) -f Dockerfile .

push-controller: build-controller
	docker push $(CONTROLLER_IMG)
	@echo "Capturing pre-rollout controller imageID (if any)..."
	@before=$$(kubectl get pod -n $(APP_NAMESPACE) -l app=dat6-controller \
	  -o jsonpath='{.items[0].status.containerStatuses[0].imageID}' 2>/dev/null); \
	echo "  before: $${before:-<no pod>}"; \
	if kubectl get deployment dat6-controller -n $(APP_NAMESPACE) >/dev/null 2>&1; then \
	  kubectl rollout restart deployment/dat6-controller -n $(APP_NAMESPACE); \
	  kubectl rollout status  deployment/dat6-controller -n $(APP_NAMESPACE) --timeout=120s; \
	  after=$$(kubectl get pod -n $(APP_NAMESPACE) -l app=dat6-controller \
	    -o jsonpath='{.items[0].status.containerStatuses[0].imageID}' 2>/dev/null); \
	  echo "  after:  $${after}"; \
	  if [ -n "$${before}" ] && [ "$${before}" = "$${after}" ]; then \
	    echo "ERROR: imageID unchanged after rollout — kubelet served a cached layer."; \
	    echo "       Try: kubectl delete pod -l app=dat6-controller -n $(APP_NAMESPACE)"; \
	    exit 1; \
	  fi; \
	  echo "  controller is running the freshly-pushed image."; \
	else \
	  echo "  (no controller deployment yet; run 'make deploy-controller STRAT=...' to install it)"; \
	fi

# --- manual deploy / undeploy: app ---
deploy:
	kubectl apply -k k8s/app/overlays/$(STRAT) -n $(APP_NAMESPACE)
	kubectl rollout status deployment/drainable-service -n $(APP_NAMESPACE) --timeout=120s

undeploy:
	kubectl delete deployment drainable-service -n $(APP_NAMESPACE) --ignore-not-found
	kubectl wait --for=delete deployment/drainable-service -n $(APP_NAMESPACE) --timeout=60s 2>/dev/null || true

# --- manual deploy / undeploy: controller ---
# `kubectl apply -k` is idempotent: SA + RBAC stay in place across strategy
# switches, only the Deployment env diff triggers a rollout. To switch
# strategies cleanly between matrix iterations the runner deletes and
# re-applies; here we leave the apply-only path for interactive use.
deploy-controller:
	kubectl apply -k k8s/controller/overlays/$(STRAT)
	kubectl rollout status deployment/dat6-controller -n $(APP_NAMESPACE) --timeout=60s

undeploy-controller:
	kubectl delete deployment dat6-controller -n $(APP_NAMESPACE) --ignore-not-found
	kubectl wait --for=delete deployment/dat6-controller -n $(APP_NAMESPACE) --timeout=60s 2>/dev/null || true

verify-controller:
	@echo "Controller pod:"
	@kubectl get pod -n $(APP_NAMESPACE) -l app=dat6-controller \
	  -o jsonpath='  {.items[0].metadata.name}  image={.items[0].spec.containers[0].image}  imageID={.items[0].status.containerStatuses[0].imageID}{"\n"}' \
	  2>/dev/null || echo "  (no controller pod)"
	@echo "Controller env (DAT6_*):"
	@kubectl exec -n $(APP_NAMESPACE) deploy/dat6-controller -- env 2>/dev/null \
	  | grep -E '^DAT6_' | sed 's/^/  /' || echo "  (none)"

# --- experiments ---
run-all-auto:
	EXP_PROFILE=$(EXP_PROFILE) bash scripts/run_all_auto.sh $(N)

run-combo:
	EXP_PROFILE=$(EXP_PROFILE) bash scripts/run_repeats.sh $(N) $(SCENARIO) $(STRAT)

# --- process control / debugging ---
stop:
	bash scripts/stop_experiment.sh

verify-env:
	@echo "Deployment overlay label:"
	@kubectl get deployment drainable-service -o jsonpath='{.metadata.labels.app\.kubernetes\.io/component}{"\n"}' 2>/dev/null | sed 's/^/  /' || echo "  (no deployment)"
	@echo "Env on first pod:"
	@kubectl exec deploy/drainable-service -- env 2>/dev/null | grep -E '^(SLEEP_MS|DAT6_)' | sed 's/^/  /' || echo "  (none)"

clean:
	rm -rf runs/
