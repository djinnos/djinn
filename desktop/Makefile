.PHONY: install dev build preview lint storybook clean help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

install: ## Install JS dependencies
	pnpm install

dev: ## Run the Vite dev server
	pnpm dev

build: ## Build the production web bundle
	pnpm build

preview: ## Preview the production build
	pnpm preview

lint: ## Run eslint
	pnpm lint

storybook: ## Run Storybook
	pnpm storybook

clean: ## Remove build artifacts
	rm -rf dist
