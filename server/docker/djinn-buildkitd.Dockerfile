# syntax=docker/dockerfile:1.7
#
# moby/buildkit:rootless extended with the three major cloud credential
# helpers (ECR, GCR/Artifact Registry, ACR) so the chart can drive BuildKit
# pushes to any of those registries with just a credHelpers entry in the
# mounted config.json.
#
# Why bake them in:
#  * BuildKit upstream has a known IRSA bug (moby/buildkit#3947, #4152,
#    #5762) — ambient pod identity isn't picked up by buildkitd itself.
#    The workaround the issues converge on is exactly this pattern:
#    ship the helper binary in the image so the AWS/GCP/Azure SDK inside
#    the helper consumes the injected token via env vars.
#  * Kaniko statically links the same three helpers; we follow the same
#    field convention so EKS/GKE/AKS users all get the same chart shape.

ARG BUILDKIT_VERSION=v0.15.2

# ── Stage 1 — fetch the three helper binaries into one staging dir ────────
FROM alpine:3.20 AS helpers

ARG ECR_CRED_HELPER_VERSION=0.9.0
ARG GCR_CRED_HELPER_VERSION=2.1.22
ARG ACR_CRED_HELPER_VERSION=0.7.0
# amd64 | arm64 — set by BuildKit from --platform; all three helpers publish
# both. Keeps the image installable on arm64 nodes (e.g. kind on Apple
# Silicon, Graviton), matching moby/buildkit's own multi-arch manifest.
ARG TARGETARCH

RUN case "${TARGETARCH}" in amd64|arm64) ;; *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; esac \
    && apk add --no-cache curl ca-certificates tar \
    && mkdir -p /helpers

RUN curl -fsSL --connect-timeout 10 --max-time 120 \
      --retry 4 --retry-delay 2 --retry-all-errors \
      -o /helpers/docker-credential-ecr-login \
      "https://amazon-ecr-credential-helper-releases.s3.amazonaws.com/${ECR_CRED_HELPER_VERSION}/linux-${TARGETARCH}/docker-credential-ecr-login"

RUN curl -fsSL --connect-timeout 10 --max-time 120 \
      --retry 4 --retry-delay 2 --retry-all-errors \
      "https://github.com/GoogleCloudPlatform/docker-credential-gcr/releases/download/v${GCR_CRED_HELPER_VERSION}/docker-credential-gcr_linux_${TARGETARCH}-${GCR_CRED_HELPER_VERSION}.tar.gz" \
    | tar -xz -C /helpers docker-credential-gcr

RUN curl -fsSL --connect-timeout 10 --max-time 120 \
      --retry 4 --retry-delay 2 --retry-all-errors \
      "https://github.com/chrismellard/docker-credential-acr-env/releases/download/${ACR_CRED_HELPER_VERSION}/docker-credential-acr-env_${ACR_CRED_HELPER_VERSION}_linux_${TARGETARCH}.tar.gz" \
    | tar -xz -C /helpers docker-credential-acr-env

RUN chmod 0755 /helpers/*

# ── Stage 2 — drop the helpers onto moby/buildkit:rootless ────────────────
FROM moby/buildkit:${BUILDKIT_VERSION}-rootless

COPY --from=helpers /helpers/docker-credential-ecr-login /usr/local/bin/
COPY --from=helpers /helpers/docker-credential-gcr       /usr/local/bin/
COPY --from=helpers /helpers/docker-credential-acr-env   /usr/local/bin/
