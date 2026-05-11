SHELL := /bin/bash
CARGO ?= cargo
PKG ?= koda-agent-cli
BIN ?= koda-agent
PORT ?= 8787
PROMPT ?= 请只回复 OK，不要调用工具。

.PHONY: help fmt fmt-check clippy test check build run docs docs-serve tmwebdriver smoke-browser smoke-tmwd-extension smoke-tmwd-matrix smoke-tmwd-static-parity smoke-rich-monitor smoke-tui smoke-http smoke-acp smoke-acp-client smoke-webhook audit-secrets audit-history release-dry-run install-local uninstall-local bootstrap-python doctor clean git-status

help:
	@printf '%s\n' \
	  'Targets:' \
	  '  make fmt          Format all Rust code' \
	  '  make fmt-check    Check formatting' \
	  '  make clippy       Run clippy with -D warnings' \
	  '  make test         Run all tests/all features' \
	  '  make check        fmt-check + test + clippy' \
	  '  make build        Build workspace' \
	  '  make run PROMPT=  Run one prompt' \
	  '  make docs         Build the Chinese mdBook tutorial site' \
	  '  make docs-serve   Serve the Chinese mdBook tutorial site locally' \
	  '  make tmwebdriver  Start TMWebDriver-compatible browser bridge master' \
	  '  make smoke-browser Verify Chrome CDP browser bridge on 127.0.0.1:9222' \
	  '  make smoke-tmwd-extension Verify installed Edge/Chrome tmwd_cdp_bridge via master' \
	  '  make smoke-tmwd-matrix Run real-page tmwd scenario matrix via installed extension' \
	  '  make smoke-tmwd-static-parity Compare tmwd bridge command surface with upstream assets' \
	  '  make smoke-rich-monitor Verify web_execute_js rich monitor on local CDP tab' \
	  '  make smoke-tui    Verify full TUI non-TTY safety and trial entrypoints' \
	  '  make smoke-http   Start HTTP frontend and test /health + /webhook' \
	  '  make smoke-acp    Test ACP JSON-RPC over stdin/stdout' \
	  '  make smoke-acp-client Test ACP via xtask external JSONL client process' \
	  '  make smoke-webhook Test stdin webhook frontend /status' \
	  '  make audit-secrets Scan current tracked files for secrets/runtime data' \
	  '  make audit-history Scan Git history for secrets/runtime data' \
	  '  make release-dry-run Build release binary and checksum locally' \
	  '  make install-local Install from this checkout into ~/.local' \
	  '  make uninstall-local Remove ~/.local/bin/koda-agent' \
	  '  make bootstrap-python Create/repair managed Python helper venv' \
	  '  make doctor       Print JSON environment diagnostics' \
	  '  make git-status   Show git status'

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

test:
	$(CARGO) test --workspace --all-features

check: fmt-check test clippy

build:
	$(CARGO) build --workspace --all-features

run:
	$(CARGO) run -p $(PKG) -- --input '$(PROMPT)'

docs:
	@command -v mdbook >/dev/null 2>&1 || { echo 'mdbook is required. Install with: cargo install mdbook'; exit 127; }
	mdbook build docs/book

docs-serve:
	@command -v mdbook >/dev/null 2>&1 || { echo 'mdbook is required. Install with: cargo install mdbook'; exit 127; }
	mdbook serve docs/book

tmwebdriver:
	$(CARGO) run -p $(PKG) -- frontend tmwebdriver


audit-secrets:
	scripts/audit-secrets.sh

audit-history:
	scripts/audit-history.sh

release-dry-run:
	$(CARGO) build -p $(PKG) --release --locked
	rm -rf dist/pkg
	mkdir -p dist/pkg
	cp target/release/$(BIN) dist/pkg/$(BIN)
	dist/pkg/$(BIN) --home dist/pkg/koda-home resources install --source . --repair >/dev/null
	cp -R dist/pkg/koda-home/resources dist/pkg/resources
	rm -rf dist/pkg/koda-home
	tar -C dist/pkg -czf dist/koda-agent-local.tar.gz $(BIN) resources
	(cd dist && shasum -a 256 koda-agent-local.tar.gz > SHA256SUMS)
	dist/pkg/$(BIN) --help >/dev/null
	dist/pkg/$(BIN) --home dist/pkg/koda-home --resource-dir dist/pkg/resources doctor --json >/dev/null

install-local:
	scripts/install.sh --from-source

uninstall-local:
	scripts/uninstall.sh

bootstrap-python:
	$(CARGO) run -p $(PKG) -- bootstrap-python --extras core --repair

doctor:
	$(CARGO) run -p $(PKG) -- doctor --json

smoke-browser:
	$(CARGO) run -p xtask -- browser-smoke

smoke-tmwd-extension:
	$(CARGO) run -p xtask -- tmwd-extension-smoke

smoke-tmwd-matrix:
	$(CARGO) run -p xtask -- tmwd-real-matrix

smoke-tmwd-static-parity:
	$(CARGO) run -p xtask -- tmwd-static-parity-smoke

smoke-rich-monitor:
	$(CARGO) run -p xtask -- rich-monitor-smoke

smoke-tui:
	$(CARGO) run -p xtask -- tui-smoke

smoke-http:
	@set -euo pipefail; \
	  LOG=$$(mktemp); \
	  KODA_FRONTEND_PORT=$(PORT) $(CARGO) run -p $(PKG) -- frontend http > $$LOG 2>&1 & \
	  PID=$$!; \
	  trap 'kill $$PID >/dev/null 2>&1 || true; rm -f $$LOG' EXIT; \
	  for _ in $$(seq 1 50); do curl -fsS http://127.0.0.1:$(PORT)/health >/tmp/koda-health.json && break || sleep 0.2; done; \
	  cat /tmp/koda-health.json; echo; \
	  curl -fsS -X POST http://127.0.0.1:$(PORT)/webhook -H 'content-type: application/json' -d '{"prompt":"/status"}'; echo; \
	  sed -n '1,20p' $$LOG

smoke-acp:
	printf '%s\n' \
	  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
	  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"'"$$(pwd)"'"}}' \
	  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"koda_default","prompt":[{"type":"text","text":"/llms"}]}}' \
	  '{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"koda_default","prompt":[{"type":"text","text":"/quit"}]}}' \
	| $(CARGO) run -p $(PKG) -- serve-acp

smoke-acp-client:
	$(CARGO) run -p xtask -- acp-client-smoke

smoke-webhook:
	printf '/status\n/quit\n' | $(CARGO) run -p $(PKG) -- frontend webhook

clean:
	$(CARGO) clean

git-status:
	git status --short
