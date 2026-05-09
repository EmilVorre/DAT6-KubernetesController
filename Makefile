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

APP_NAMESPACE ?= default
N            ?= 5
STRAT        ?= baseline
SCENARIO     ?= rollout
EXP_PROFILE  ?= thesis-stress

.PHONY: help build-app push-app deploy undeploy run-all-auto run-combo stop verify-env clean

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
	@echo "  make build-app                   docker build"
	@echo "  make push-app                    docker push to GHCR"
	@echo ""
	@echo "Manual deploy:"
	@echo "  make deploy STRAT=baseline       Apply overlay (no controller, no test)"
	@echo "  make undeploy                    Delete deployment"
	@echo ""
	@echo "Profile (env var; override on command line):"
	@echo "  EXP_PROFILE=standard make run-all-auto N=1"
	@echo "  K6_RPS=400 K6_VUS=400 make run-all-auto N=3"

# --- image ---
build-app:
	docker build -t ghcr.io/emilvorre/drainable-service:latest -f app/Dockerfile app/

push-app: build-app
	docker push ghcr.io/emilvorre/drainable-service:latest

# --- manual deploy / undeploy ---
deploy:
	kubectl apply -k k8s/overlays/$(STRAT) -n $(APP_NAMESPACE)
	kubectl rollout status deployment/drainable-service -n $(APP_NAMESPACE) --timeout=120s

undeploy:
	kubectl delete deployment drainable-service -n $(APP_NAMESPACE) --ignore-not-found
	kubectl wait --for=delete deployment/drainable-service -n $(APP_NAMESPACE) --timeout=60s 2>/dev/null || true

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
