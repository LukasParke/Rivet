.PHONY: help dev backend web docker-build docker-run docker-stop test clean secret-key backend-dry-run backend-watch install-web-deps build-web test-verbose kill-backend fmt lint

help: ## Show this help message
	@echo 'Usage: make [target]'
	@echo ''
	@echo 'Available targets:'
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-15s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

SECRET_KEY_FILE := .rivet-secret-key
DEV_DB := rivet-dev.db

secret-key: ## Generate a dev secret key (32 bytes, base64)
	@python3 -c "import base64, os; print(base64.b64encode(os.urandom(32)).decode())"

ensure-secret-key: ## Ensure a persistent secret key exists (creates .rivet-secret-key if missing)
	@if [ ! -f $(SECRET_KEY_FILE) ]; then \
		echo "Creating persistent secret key file: $(SECRET_KEY_FILE)"; \
		make secret-key > $(SECRET_KEY_FILE); \
		echo "Secret key saved to $(SECRET_KEY_FILE)"; \
	fi

backend: ensure-secret-key ## Start the Rust API/WS server (port 8080)
	@if lsof -ti:8080 >/dev/null 2>&1; then \
		echo "Error: Port 8080 is already in use. Kill the process or use a different port:"; \
		echo "  lsof -ti:8080 | xargs kill"; \
		echo "  or: HTTP_ADDR=:8081 make backend"; \
		exit 1; \
	fi
	@APP_ENV=$${APP_ENV:-dev} \
	HTTP_ADDR=$${HTTP_ADDR:-:8080} \
	DATABASE_URL=$${DATABASE_URL:-file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)} \
	APP_SECRET_KEY=$${APP_SECRET_KEY:-$$(cat $(SECRET_KEY_FILE) 2>/dev/null || echo "")} \
	cargo run --bin rivet-server

backend-watch: ensure-secret-key ## Start the API server with hot reload (cargo-watch)
	@command -v cargo-watch >/dev/null 2>&1 || { echo "Install cargo-watch: cargo install cargo-watch"; exit 1; }
	@APP_ENV=dev \
	HTTP_ADDR=:8080 \
	DATABASE_URL=file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL) \
	APP_SECRET_KEY=$$(cat $(SECRET_KEY_FILE)) \
	cargo watch -x 'run --bin rivet-server'

web: ## Start the SvelteKit dev server (port 5173)
	npm -C apps/web run dev

dev: ensure-secret-key ## Start both backend and web in parallel
	@make backend & make web & wait

backend-dry-run: ensure-secret-key ## Start backend with DRY_RUN=1 enabled
	@DRY_RUN=1 APP_ENV=dev \
	HTTP_ADDR=:8080 \
	DATABASE_URL=file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL) \
	APP_SECRET_KEY=$$(cat $(SECRET_KEY_FILE)) \
	cargo run --bin rivet-server

docker-build: ## Build the Docker image
	docker build -t rivet:dev .

docker-run: ## Run the Docker container (requires docker-build first)
	docker run --rm -p 8080:8080 -v "$$PWD/data:/data" \
		-e APP_ENV=prod \
		-e APP_SECRET_KEY=$$(python3 -c 'import base64,os;print(base64.b64encode(os.urandom(32)).decode())') \
		rivet:dev

docker-stop: ## Stop any running Docker containers
	-docker stop $$(docker ps -q --filter ancestor=rivet:dev)

test: ## Run all Rust tests
	cargo test

test-verbose: ## Run Rust tests with verbose output
	cargo test -- --nocapture

fmt: ## Format Rust and web code
	cargo fmt
	npm -C apps/web exec prettier -- --write src 2>/dev/null || true

lint: ## Clippy + svelte-check
	cargo clippy --all-targets -- -D warnings
	npm -C apps/web run check

kill-backend: ## Kill any process using port 8080
	-lsof -ti:8080 | xargs kill -9

clean: ## Clean build artifacts and node_modules
	cargo clean
	rm -rf apps/web/node_modules apps/web/.svelte-kit apps/web/build
	rm -f rivet-dev.db rivet-dev.db-shm rivet-dev.db-wal

install-web-deps: ## Install web dependencies
	cd apps/web && npm ci

build-web: ## Build the web app for production
	cd apps/web && npm run build

goldens: ## Regenerate committed golden artifacts (charte OpenAPI, parser, api routes)
	./scripts/update-goldens.sh

golden-check: ## Assert committed goldens match source of truth
	cargo test --test parser_golden --test api_golden
