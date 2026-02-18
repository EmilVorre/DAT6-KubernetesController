# DAT6-KubernetesController — baseline + testbed
# Reproducible experiments for safe container decomposition research

CLUSTER_NAME ?= dat6-testbed
KIND_CONFIG ?= kind/cluster.yaml
K8S_BASE ?= k8s/base
K8S_OVERLAY ?= baseline
SCENARIO ?= steady_scale_down
STRAT ?= baseline
RUN_DIR ?= runs
TIMESTAMP ?= $(shell date +%Y%m%d-%H%M%S)
APP_IMAGE ?= drainable-service:latest
APP_NAMESPACE ?= default

.PHONY: cluster-up cluster-down cluster-status
.PHONY: deploy-baseline deploy-prometheus deploy-kube-state-metrics
.PHONY: build-app load-app run run-repeats clean help

# --- Cluster lifecycle ---
cluster-up:
	kind create cluster --name $(CLUSTER_NAME) --config $(KIND_CONFIG)
	@echo "Cluster $(CLUSTER_NAME) is up. Run 'make deploy-prometheus' then 'make deploy-baseline'."

cluster-down:
	kind delete cluster --name $(CLUSTER_NAME)

cluster-status:
	kubectl cluster-info --context kind-$(CLUSTER_NAME)
	kubectl get nodes -o wide

# --- Observability stack (Helm) ---
deploy-prometheus:
	helm repo add prometheus-community https://prometheus-community.github.io/helm-charts 2>/dev/null || true
	helm repo update
	helm upgrade --install prometheus prometheus-community/kube-prometheus-stack \
		--namespace observability --create-namespace \
		-f observability/prometheus/values.yaml \
		--wait --timeout 5m

deploy-kube-state-metrics:
	@echo "kube-state-metrics is included in kube-prometheus-stack. Run 'make deploy-prometheus'."

# --- Build & load drainable service ---
build-app:
	docker build -t $(APP_IMAGE) -f app/Dockerfile app/

load-app:
	kind load docker-image $(APP_IMAGE) --name $(CLUSTER_NAME)

# --- Deploy baseline workload ---
deploy-baseline: build-app load-app
	kubectl apply -k k8s/overlays/$(K8S_OVERLAY) -n $(APP_NAMESPACE)
	@echo "Waiting for deployment..."
	kubectl rollout status deployment/drainable-service -n $(APP_NAMESPACE) --timeout=120s

undeploy-baseline:
	kubectl delete -k k8s/overlays/$(K8S_OVERLAY) --ignore-not-found --wait=false

# --- Run scenario ---
run:
	bash scripts/run_scenario.sh $(SCENARIO) $(STRAT) $(RUN_DIR)/$(TIMESTAMP)-$(SCENARIO)-$(STRAT)

# --- Run scenario N times and aggregate metrics ---
N ?= 5
run-repeats:
	bash scripts/run_repeats.sh $(N) $(SCENARIO) $(STRAT)

# --- Full setup (cluster + prometheus + baseline) ---
setup: cluster-up
	@echo "Waiting for cluster to be ready..."
	sleep 15
	$(MAKE) deploy-prometheus
	$(MAKE) deploy-baseline K8S_OVERLAY=baseline

# --- Cleanup ---
clean:
	$(MAKE) undeploy-baseline 2>/dev/null || true
	$(MAKE) cluster-down 2>/dev/null || true

help:
	@echo "DAT6 Baseline + Testbed"
	@echo ""
	@echo "Cluster:"
	@echo "  make cluster-up          Create kind cluster"
	@echo "  make cluster-down        Delete kind cluster"
	@echo "  make cluster-status      Show cluster info"
	@echo ""
	@echo "Deploy:"
	@echo "  make deploy-prometheus   Install Prometheus stack"
	@echo "  make deploy-baseline     Build, load, deploy drainable service"
	@echo "  make deploy-baseline K8S_OVERLAY=long-requests"
	@echo "  make deploy-baseline K8S_OVERLAY=burst"
	@echo ""
	@echo "Run:"
	@echo "  make run SCENARIO=steady_scale_down STRAT=baseline"
	@echo "  make run SCENARIO=steady_scale_down STRAT=long-requests"
	@echo "  make run SCENARIO=rollout STRAT=s1-early-readiness"
	@echo ""
	@echo "Run N repeats (stable metrics):"
	@echo "  make run-repeats N=5 SCENARIO=steady_scale_down STRAT=baseline"
	@echo "  make run-repeats N=5 SCENARIO=rollout STRAT=s1-early-readiness"
	@echo ""
	@echo "Full setup:"
	@echo "  make setup               cluster-up + prometheus + baseline"
