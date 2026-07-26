# nudo — development and demo commands.
#
#   make help          what everything does
#   make demo          a running nudo with a real target and three services
#   make check         what CI runs
#
# The demo targets spin up two containers: nudo itself, and a systemd host it
# deploys to over SSH. That second one is the point — without a real systemd
# target you can look at the dashboard but not actually deploy anything.

.DEFAULT_GOAL := help
SHELL := /usr/bin/env bash
.SHELLFLAGS := -eu -o pipefail -c

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Where the demo publishes.
WEB_PORT   ?= 3000
GRPC_PORT  ?= 50051

# Container and network names, so the demo cannot collide with anything of
# yours.
IMAGE            ?= nudo:dev
NUDO_CONTAINER   ?= nudo-demo
TARGET_CONTAINER ?= nudo-demo-target
DEMO_NETWORK     ?= nudo-demo-net
DEMO_VOLUME      ?= nudo-demo-state

# The image the demo target runs. Debian with systemd as PID 1 is the closest
# thing to the hosts nudo actually deploys to.
TARGET_IMAGE ?= debian:bookworm

# Scratch state: the demo account's cookies, the target's SSH key, staged
# artifacts. Git-ignored.
STATE_DIR ?= .nudo-demo

# On Docker Desktop the control plane resolves the host by this name. On Linux
# the run below adds it explicitly.
ARTIFACT_HOST ?= host.docker.internal
ARTIFACT_PORT ?= 8099

NUDO_URL := http://127.0.0.1:$(WEB_PORT)

export NUDO_URL NUDO_CONTAINER TARGET_CONTAINER STATE_DIR ARTIFACT_HOST ARTIFACT_PORT
export NUDO_ENDPOINT := http://127.0.0.1:$(GRPC_PORT)

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------

.PHONY: help
help: ## Show this help
	@printf '\033[1mnudo\033[0m — deploy bare-metal binaries over SSH and systemd\n\n'
	@printf '\033[1mGetting started\033[0m\n'
	@printf '  make demo          spin up nudo + a systemd target + three example services\n'
	@printf '  make demo-open     print the URL and the demo credentials\n'
	@printf '  make demo-down     tear it all down\n\n'
	@printf '\033[1mAll commands\033[0m\n'
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@printf '\n\033[1mExamples\033[0m (make example-deploy EXAMPLE=<name>)\n'
	@for dir in examples/services/*/; do \
		name=$$(basename $$dir); \
		desc=$$(python3 -c "import json;print(json.load(open('$$dir/service.json')).get('description',''))" 2>/dev/null || echo ''); \
		printf "  \033[36m%-20s\033[0m %s\n" "$$name" "$$desc"; \
	done

# ---------------------------------------------------------------------------
# Build and test
# ---------------------------------------------------------------------------

.PHONY: build
build: ## Build all binaries (debug)
	cargo build --workspace

.PHONY: release
release: ## Build all binaries (release)
	cargo build --workspace --release

.PHONY: test
test: ## Run the unit and integration tests
	cargo test --workspace

.PHONY: test-e2e
test-e2e: ## Run the end-to-end deployment tests (needs Docker, ~1 min)
	cargo test -p nudo-server --features e2e --test e2e -- --test-threads=1 --nocapture

.PHONY: fmt
fmt: ## Format the workspace
	cargo fmt --all

.PHONY: lint
lint: ## Check formatting and run clippy, as CI does
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: test-scripts
test-scripts: ## Test the release-publishing script
	python3 scripts/add_release_test.py

.PHONY: check
check: lint test test-scripts ## Everything CI runs, short of the e2e suite
	@printf '\033[1;32m✓\033[0m fmt, clippy and tests all pass\n'

.PHONY: image
image: ## Build the Docker image
	docker build -t $(IMAGE) .

# ---------------------------------------------------------------------------
# The demo
# ---------------------------------------------------------------------------

.PHONY: demo
demo: image demo-up demo-target demo-examples demo-open ## Full demo: nudo, a target, and the example services

.PHONY: demo-up
demo-up: $(STATE_DIR)/secret.key ## Start nudo and a systemd target
	@docker network inspect $(DEMO_NETWORK) > /dev/null 2>&1 \
		|| docker network create $(DEMO_NETWORK) > /dev/null
	@if [ -z "$$(docker ps -q -f name=^$(NUDO_CONTAINER)$$)" ]; then \
		printf '\033[1;36m==>\033[0m starting nudo\n'; \
		docker rm -f $(NUDO_CONTAINER) > /dev/null 2>&1 || true; \
		docker run -d --name $(NUDO_CONTAINER) \
			--network $(DEMO_NETWORK) \
			--add-host $(ARTIFACT_HOST):host-gateway \
			-p $(WEB_PORT):3000 -p $(GRPC_PORT):50051 \
			-e NUDO_SECRET_KEY="$$(cat $(STATE_DIR)/secret.key)" \
			-e NUDO_BASE_URL="$(NUDO_URL)" \
			-e NUDO_GRPC_ADDR="0.0.0.0:50051" \
			-e RUST_LOG="info" \
			-v $(DEMO_VOLUME):/var/lib/nudo \
			$(IMAGE) > /dev/null; \
	else \
		printf '\033[1;36m==>\033[0m nudo is already running\n'; \
	fi
	@$(MAKE) --no-print-directory demo-target-container
	@printf '\033[1;36m==>\033[0m waiting for nudo\n'
	@for i in $$(seq 1 60); do \
		curl -fsS $(NUDO_URL)/login > /dev/null 2>&1 && break; \
		sleep 1; \
	done
	@curl -fsS $(NUDO_URL)/login > /dev/null 2>&1 \
		|| { printf '\033[1;31mx\033[0m nudo did not come up — try: make demo-logs\n'; exit 1; }

.PHONY: demo-target-container
demo-target-container:
	@if [ -z "$$(docker ps -q -f name=^$(TARGET_CONTAINER)$$)" ]; then \
		printf '\033[1;36m==>\033[0m starting a systemd target (installs sshd, ~30s)\n'; \
		docker rm -f $(TARGET_CONTAINER) > /dev/null 2>&1 || true; \
		docker run -d --name $(TARGET_CONTAINER) \
			--network $(DEMO_NETWORK) \
			--privileged --cgroupns=host \
			-v /sys/fs/cgroup:/sys/fs/cgroup:rw \
			$(TARGET_IMAGE) \
			/bin/bash -c 'apt-get update -qq \
				&& DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
					systemd systemd-sysv openssh-server curl procps util-linux \
					netcat-openbsd >/dev/null \
				&& mkdir -p /run/sshd \
				&& exec /lib/systemd/systemd' > /dev/null; \
		for i in $$(seq 1 90); do \
			docker exec $(TARGET_CONTAINER) systemctl is-system-running 2>/dev/null \
				| grep -qE 'running|degraded' && break; \
			sleep 2; \
		done; \
	else \
		printf '\033[1;36m==>\033[0m the target is already running\n'; \
	fi

# The key is generated once and reused, so restarting the demo does not orphan
# every secret already in the database.
$(STATE_DIR)/secret.key:
	@mkdir -p $(STATE_DIR)
	@openssl rand -hex 32 > $@
	@chmod 600 $@
	@printf '\033[1;36m==>\033[0m generated a secret-store key at $@\n'

.PHONY: demo-target
demo-target: ## Register the demo container as a nudo target
	@bash examples/scripts/setup-target.sh

.PHONY: demo-examples
demo-examples: ## Deploy every example service
	@for dir in examples/services/*/; do \
		name=$$(basename $$dir); \
		bash examples/scripts/deploy-example.sh $$name || true; \
	done

.PHONY: example-deploy
example-deploy: ## Deploy one example (EXAMPLE=hello-http)
	@test -n "$(EXAMPLE)" || { printf 'set EXAMPLE=<name> — see: make help\n'; exit 1; }
	@bash examples/scripts/deploy-example.sh $(EXAMPLE)

.PHONY: example-break
example-break: ## Ship a broken release of `flaky` and watch it roll back
	@printf '\033[1;36m==>\033[0m deploying a release that starts but never becomes ready\n'
	@printf '    systemd will report it active; only the health check notices\n\n'
	@bash examples/scripts/deploy-example.sh flaky --env HEALTHY=0 --env APP_VERSION=broken \
		|| printf '\n\033[1;32m✓\033[0m rolled back, as intended — see the deployment in the dashboard\n'

.PHONY: example-unit
example-unit: ## Print the unit file an example would write (EXAMPLE=hello-http)
	@test -n "$(EXAMPLE)" || { printf 'set EXAMPLE=<name>\n'; exit 1; }
	@svc=$$($(MAKE) --no-print-directory .service-id NAME=$(EXAMPLE)); \
		test -n "$$svc" || { printf 'deploy it first: make example-deploy EXAMPLE=$(EXAMPLE)\n'; exit 1; }; \
		$(MAKE) --no-print-directory .cli ARGS="services unit $$svc"

.PHONY: example-logs
example-logs: ## Follow an example's logs (EXAMPLE=hello-http)
	@test -n "$(EXAMPLE)" || { printf 'set EXAMPLE=<name>\n'; exit 1; }
	@svc=$$($(MAKE) --no-print-directory .service-id NAME=$(EXAMPLE)); \
		test -n "$$svc" || { printf 'deploy it first: make example-deploy EXAMPLE=$(EXAMPLE)\n'; exit 1; }; \
		$(MAKE) --no-print-directory .cli ARGS="logs $$svc --follow"

.PHONY: example-rollback
example-rollback: ## Roll an example back one release (EXAMPLE=hello-http)
	@test -n "$(EXAMPLE)" || { printf 'set EXAMPLE=<name>\n'; exit 1; }
	@svc=$$($(MAKE) --no-print-directory .service-id NAME=$(EXAMPLE)); \
		test -n "$$svc" || { printf 'deploy it first\n'; exit 1; }; \
		$(MAKE) --no-print-directory .cli ARGS="rollback $$svc --wait"

# ---------------------------------------------------------------------------
# Inspecting the demo
# ---------------------------------------------------------------------------

.PHONY: demo-open
demo-open: ## Print the dashboard URL and the demo credentials
	@printf '\n\033[1mnudo is running\033[0m\n\n'
	@printf '  dashboard  \033[36m%s\033[0m\n' '$(NUDO_URL)'
	@printf '  email      admin@example.com\n'
	@printf '  password   correct horse battery staple\n\n'
	@printf '  api        %s (for the CLI)\n' '$(NUDO_ENDPOINT)'
	@printf '\n\033[1mWorth a look\033[0m\n'
	@printf '  the unit file a deploy writes    make example-unit EXAMPLE=latency-critical\n'
	@printf '  a rollback, live                 make example-break\n'
	@printf '  logs streaming over SSE          make example-logs EXAMPLE=latency-critical\n'
	@printf '  a shell on the target            make demo-shell\n'
	@printf '  the update banner and changelog  make demo-changelog\n\n'

.PHONY: demo-changelog
demo-changelog: ## Pretend a newer release exists, to see the update banner
	@# The dashboard reads whatever the release check last recorded, so seeding
	@# that one row shows the banner and the changelog without publishing
	@# anything or reaching the network.
	@#
	@# sqlite3 is deliberately not in nudo's image — it is a deploy tool, not a
	@# database shell — so the write goes through a throwaway container on the
	@# same volume, running as the uid that owns the database.
	@mkdir -p $(STATE_DIR)
	@printf 'A pretend release, so the banner has something to show.\n\n- Something added\n- Something fixed\n\nNothing here was really published.\n' \
		> $(STATE_DIR)/fake-notes.md
	@python3 scripts/add-release.py --version 99.0.0 \
		--url 'https://github.com/loa212/nudo/releases/tag/v99.0.0' \
		--notes-file $(STATE_DIR)/fake-notes.md \
		--manifest $(STATE_DIR)/fake-manifest.json \
		--published-at 2030-01-01 > /dev/null
	@# Root installs sqlite, then drops to nudo's uid for the write itself, so
	@# the WAL files it creates stay owned by the user the server runs as.
	@docker run --rm \
		-v $(DEMO_VOLUME):/var/lib/nudo \
		-v "$$PWD/$(STATE_DIR)":/seed:ro \
		alpine:3.20 sh -c 'apk add --no-cache sqlite su-exec > /dev/null 2>&1 && \
			su-exec 10001:10001 \
			sqlite3 /var/lib/nudo/nudo.db \
			"INSERT INTO release_check (id, latest_version, manifest, checked_at, enabled) \
			 VALUES (1, '"'"'99.0.0'"'"', CAST(readfile('"'"'/seed/fake-manifest.json'"'"') AS TEXT), datetime('"'"'now'"'"'), 1) \
			 ON CONFLICT (id) DO UPDATE SET latest_version = '"'"'99.0.0'"'"', \
			 manifest = CAST(readfile('"'"'/seed/fake-manifest.json'"'"') AS TEXT), checked_at = datetime('"'"'now'"'"');"'
	@printf '\033[1;32m✓\033[0m seeded — the banner is on \033[36m%s\033[0m, the notes at \033[36m%s/changelog\033[0m\n' \
		'$(NUDO_URL)' '$(NUDO_URL)'
	@printf '  undo it with: make demo-unchangelog\n'

.PHONY: demo-unchangelog
demo-unchangelog: ## Clear the pretend release seeded by demo-changelog
	@docker run --rm -v $(DEMO_VOLUME):/var/lib/nudo \
		alpine:3.20 sh -c 'apk add --no-cache sqlite su-exec > /dev/null 2>&1 && \
			su-exec 10001:10001 \
			sqlite3 /var/lib/nudo/nudo.db "DELETE FROM release_check;"'
	@printf '\033[1;32m✓\033[0m cleared\n'

.PHONY: demo-status
demo-status: ## Show the demo's targets and services
	@printf '\033[1mtargets\033[0m\n'
	@$(MAKE) --no-print-directory .cli ARGS="targets list" || true
	@printf '\n\033[1mservices\033[0m\n'
	@$(MAKE) --no-print-directory .cli ARGS="services list" || true
	@printf '\n\033[1mrecent deployments\033[0m\n'
	@$(MAKE) --no-print-directory .cli ARGS="audit --limit 8" || true

.PHONY: demo-logs
demo-logs: ## Tail nudo's own container logs
	@docker logs -f --tail 60 $(NUDO_CONTAINER)

.PHONY: demo-shell
demo-shell: ## Open a shell on the demo target (as nudo would)
	@docker exec -it $(TARGET_CONTAINER) bash

.PHONY: demo-units
demo-units: ## Show what is actually running on the target
	@docker exec $(TARGET_CONTAINER) systemctl list-units 'hello-http*' 'latency-critical*' 'flaky*' \
		--no-pager --no-legend || true
	@printf '\n\033[1mscheduling actually applied\033[0m\n'
	@docker exec $(TARGET_CONTAINER) sh -c \
		'for u in latency-critical hello-http flaky; do \
			systemctl is-active $$u.service >/dev/null 2>&1 || continue; \
			printf "  %-18s " $$u; \
			systemctl show -p CPUAffinity -p Nice -p IOSchedulingClass $$u.service \
				| tr "\n" " "; echo; \
		done' 2>/dev/null || true

.PHONY: demo-down
demo-down: ## Stop the demo and remove its containers
	@printf '\033[1;36m==>\033[0m stopping the demo\n'
	@bash -c 'source examples/scripts/lib.sh 2>/dev/null && stop_artifact_server' 2>/dev/null || true
	@docker rm -f $(NUDO_CONTAINER) $(TARGET_CONTAINER) > /dev/null 2>&1 || true
	@docker network rm $(DEMO_NETWORK) > /dev/null 2>&1 || true
	@printf '    containers removed; the database volume is kept\n'
	@printf '    to wipe it too: make demo-clean\n'

.PHONY: demo-clean
demo-clean: demo-down ## Tear the demo down and delete its data
	@docker volume rm $(DEMO_VOLUME) > /dev/null 2>&1 || true
	@rm -rf $(STATE_DIR)
	@printf '\033[1;36m==>\033[0m removed the volume and $(STATE_DIR)\n'

.PHONY: demo-restart
demo-restart: demo-down demo-up ## Restart the demo, keeping its data
	@# The target container is recreated from scratch while nudo's database
	@# survives, so the registration outlives the authorized_keys it depends on.
	@# Reinstalling the key here is what keeps a restarted demo deployable.
	@$(MAKE) --no-print-directory demo-target

# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

# Runs the CLI against the demo, preferring a release build.
.PHONY: .cli
.cli:
	@if [ -x target/release/nudo ]; then bin=target/release/nudo; \
	elif [ -x target/debug/nudo ]; then bin=target/debug/nudo; \
	else printf 'the CLI is not built — run: make build\n' >&2; exit 1; fi; \
	$$bin --endpoint $(NUDO_ENDPOINT) $(ARGS)

# Resolves a service id by name, for the example-* targets.
.PHONY: .service-id
.service-id:
	@if [ -x target/release/nudo ]; then bin=target/release/nudo; \
	elif [ -x target/debug/nudo ]; then bin=target/debug/nudo; \
	else exit 0; fi; \
	$$bin --endpoint $(NUDO_ENDPOINT) services list --output json 2>/dev/null \
		| python3 -c "import json,sys; \
			services=json.load(sys.stdin)['services']; \
			print(next((s['id'] for s in services if s['name']=='$(NAME)'), ''))" 2>/dev/null || true
