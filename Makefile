.PHONY: help dev backend web docker-build docker-run docker-stop test clean secret-key backend-dry-run backend-watch install-web-deps build-web test-verbose kill-backend

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

backend: ## Start the Go API/WS server (port 8080)
	@if lsof -ti:8080 >/dev/null 2>&1; then \
		echo "Error: Port 8080 is already in use. Kill the process or use a different port:"; \
		echo "  lsof -ti:8080 | xargs kill"; \
		echo "  or: HTTP_ADDR=:8081 make backend"; \
		exit 1; \
	fi
	@if [ -z "$$APP_SECRET_KEY" ]; then \
		if [ -f $(SECRET_KEY_FILE) ]; then \
			export APP_SECRET_KEY=$$(cat $(SECRET_KEY_FILE)); \
		else \
			echo "Warning: APP_SECRET_KEY not set and $(SECRET_KEY_FILE) not found."; \
			echo "Run 'make ensure-secret-key' to create a persistent key, or 'make secret-key' and export it."; \
		fi; \
	fi
	@APP_ENV=$${APP_ENV:-dev} \
	HTTP_ADDR=$${HTTP_ADDR:-:8080} \
	DATABASE_URL=$${DATABASE_URL:-file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)} \
	APP_SECRET_KEY=$${APP_SECRET_KEY:-$$(cat $(SECRET_KEY_FILE) 2>/dev/null || echo "")} \
	go run ./cmd/server

backend-watch: ensure-secret-key ## Start the Go API/WS server with hot reload (port 8080)
	@if lsof -ti:8080 >/dev/null 2>&1; then \
		echo "Error: Port 8080 is already in use. Kill the process or use a different port:"; \
		echo "  lsof -ti:8080 | xargs kill"; \
		echo "  or: HTTP_ADDR=:8081 make backend-watch"; \
		exit 1; \
	fi
	@if ! command -v air >/dev/null 2>&1 && [ ! -f ~/go/bin/air ] && [ ! -f $$HOME/go/bin/air ]; then \
		echo "Error: air not found. Install it with: go install github.com/air-verse/air@latest"; \
		echo "Then ensure ~/go/bin is in your PATH, or use the full path."; \
		exit 1; \
	fi
	@echo "Starting backend with hot reload..."
	@echo "Using persistent secret key from $(SECRET_KEY_FILE)"
	@echo "Using database: $(DEV_DB)"
	@PATH=$${PATH}:$${HOME}/go/bin \
	APP_ENV=$${APP_ENV:-dev} \
	HTTP_ADDR=$${HTTP_ADDR:-:8080} \
	DATABASE_URL=$${DATABASE_URL:-file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)} \
	APP_SECRET_KEY=$$(cat $(SECRET_KEY_FILE)) \
	air

web: ## Start the SvelteKit dev server (port 5173)
	@cd apps/web && npm run dev

dev: ensure-secret-key ## Start both backend and web in parallel
	@echo "Starting backend and web servers..."
	@echo "Backend will be at http://localhost:8080"
	@echo "Web UI will be at http://localhost:5173"
	@echo "Using persistent secret key from $(SECRET_KEY_FILE)"
	@echo "Using database: $(DEV_DB)"
	@APP_ENV=$${APP_ENV:-dev} \
	HTTP_ADDR=$${HTTP_ADDR:-:8080} \
	DATABASE_URL=$${DATABASE_URL:-file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)} \
	APP_SECRET_KEY=$$(cat $(SECRET_KEY_FILE)) \
	DRY_RUN=$${DRY_RUN:-1} \
	sh -c 'trap "kill 0" EXIT; (make backend &) && (sleep 2 && make web &) && wait'

backend-dry-run: ensure-secret-key ## Start backend with DRY_RUN=1 enabled
	@if lsof -ti:8080 >/dev/null 2>&1; then \
		echo "Error: Port 8080 is already in use. Kill the process or use a different port:"; \
		echo "  lsof -ti:8080 | xargs kill"; \
		echo "  or: HTTP_ADDR=:8081 make backend-dry-run"; \
		exit 1; \
	fi
	@echo "Using persistent secret key from $(SECRET_KEY_FILE)"
	@echo "Using database: $(DEV_DB)"
	@APP_ENV=$${APP_ENV:-dev} \
	HTTP_ADDR=$${HTTP_ADDR:-:8080} \
	DATABASE_URL=$${DATABASE_URL:-file:$(DEV_DB)?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)} \
	APP_SECRET_KEY=$$(cat $(SECRET_KEY_FILE)) \
	DRY_RUN=1 \
	go run ./cmd/server

docker-build: ## Build the Docker image
	docker build -t rivet:dev .

docker-run: ## Run the Docker container (requires docker-build first)
	@if [ -z "$$APP_SECRET_KEY" ]; then \
		export APP_SECRET_KEY=$$(make secret-key); \
	fi
	@docker run --rm -p 8080:8080 -v "$$PWD/data:/data" \
		-e APP_ENV=prod \
		-e APP_SECRET_KEY="$$APP_SECRET_KEY" \
		rivet:dev

docker-stop: ## Stop any running Docker containers
	@docker ps -q --filter "ancestor=rivet:dev" | xargs -r docker stop

test: ## Run Go tests
	go test ./...

test-verbose: ## Run Go tests with verbose output
	go test -v ./...

kill-backend: ## Kill any process using port 8080
	@if lsof -ti:8080 >/dev/null 2>&1; then \
		echo "Killing process on port 8080..."; \
		lsof -ti:8080 | xargs kill; \
		sleep 1; \
		if lsof -ti:8080 >/dev/null 2>&1; then \
			echo "Force killing..."; \
			lsof -ti:8080 | xargs kill -9; \
		fi; \
		echo "Port 8080 is now free."; \
	else \
		echo "No process found on port 8080."; \
	fi

clean: ## Clean build artifacts and node_modules
	rm -rf apps/web/build apps/web/node_modules
	rm -f rivet-dev.db*
	@echo "Note: .rivet-secret-key is preserved. Delete it manually if you want to reset secrets."

install-web-deps: ## Install web dependencies
	cd apps/web && npm install

build-web: ## Build the web app for production
	cd apps/web && npm run build
