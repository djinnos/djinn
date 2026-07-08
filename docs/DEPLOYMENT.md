# Deploying djinn

**This guide moved and grew into a set of per-environment guides under
[`docs/deploy/`](deploy/README.md).**

| You're looking for | Now lives at |
|--------------------|--------------|
| Overview: requirements, bundled vs external services, how installs/upgrades/migrations work, image tags, node prerequisites | [deploy/README.md](deploy/README.md) |
| Single node / self-hosted / VPS (k3s, everything bundled) | [deploy/vps.md](deploy/vps.md) |
| Managed or self-managed Kubernetes (EKS / GKE / AKS / kubeadm) | [deploy/kubernetes.md](deploy/kubernetes.md) |
| Values reference, external Postgres (RDS / Cloud SQL), registries, secrets, storage, dispatch-state debug endpoint | [deploy/configuration.md](deploy/configuration.md) |
| AI-assisted install (paste-a-prompt) | [deploy/AGENT.md](deploy/AGENT.md) |
| Pre-task lifecycle hooks: database migrations, validation constraints, failure policies, rollout sequencing | [deploy/lifecycle-pre-task.md](deploy/lifecycle-pre-task.md) |
