#!/usr/bin/env bash
# Deploy the provisioning service to Cloud Run.
#
# This is the machine that holds the cloud credential, so that no user's Mac
# has to. Run it from the blaude-agent repo root:
#
#   bash deploy/provision-api/deploy.sh
#
# Idempotent: every step checks before it creates, so re-running it is how you
# ship a new version.
set -euo pipefail

PROJECT="${BLAUDE_PROJECT:-enclave-money}"
REGION="${BLAUDE_PROVISION_REGION:-asia-south1}"
SERVICE="${BLAUDE_PROVISION_SERVICE:-blaude-provision-api}"
SA="blaude-provision"
SA_EMAIL="${SA}@${PROJECT}.iam.gserviceaccount.com"
SSH_SECRET="blaude-provision-ssh-key"

say() { printf '\n== %s\n' "$*"; }

command -v gcloud >/dev/null || { echo "gcloud is not installed"; exit 1; }
gcloud auth print-access-token >/dev/null 2>&1 || {
  echo "gcloud is not signed in. Run: gcloud auth login"; exit 1; }

say "APIs"
gcloud services enable \
  run.googleapis.com cloudbuild.googleapis.com compute.googleapis.com \
  secretmanager.googleapis.com artifactregistry.googleapis.com \
  --project "$PROJECT" --quiet

say "service account"
gcloud iam service-accounts describe "$SA_EMAIL" --project "$PROJECT" >/dev/null 2>&1 || \
  gcloud iam service-accounts create "$SA" \
    --project "$PROJECT" \
    --display-name "blaude provisioning (builds team servers)" --quiet

# compute.admin: create/delete instances, addresses and firewall rules.
# iam.serviceAccountUser: attach the default compute SA to a new instance.
# secretmanager.secretAccessor: read the SSH key below.
for role in roles/compute.admin roles/iam.serviceAccountUser roles/secretmanager.secretAccessor; do
  say "role $role"
  gcloud projects add-iam-policy-binding "$PROJECT" \
    --member "serviceAccount:${SA_EMAIL}" --role "$role" \
    --condition=None --quiet >/dev/null
done

# A STABLE ssh key.
#
# `gcloud compute ssh` generates one on first use and registers it. On Cloud
# Run the filesystem is ephemeral, so every cold start would mint another key
# and push it to project metadata — the key list would grow without bound and
# each first request would pay the registration round trip. One key, kept in
# Secret Manager, mounted at startup, avoids both.
say "ssh key"
if ! gcloud secrets describe "$SSH_SECRET" --project "$PROJECT" >/dev/null 2>&1; then
  tmp="$(mktemp -d)"
  ssh-keygen -t ed25519 -N "" -C "blaude-provision" -f "$tmp/key" >/dev/null
  gcloud secrets create "$SSH_SECRET" --project "$PROJECT" \
    --data-file="$tmp/key" --quiet
  # Register the public half for the service account's login name.
  gcloud compute os-login ssh-keys add \
    --key-file="$tmp/key.pub" --project "$PROJECT" --quiet 2>/dev/null \
    || echo "   (os-login not in use; the key will register on first ssh)"
  rm -rf "$tmp"
  echo "   created $SSH_SECRET"
else
  echo "   $SSH_SECRET already exists"
fi

say "Clerk"
: "${CLERK_JWKS_URL:=}"
if [ -z "$CLERK_JWKS_URL" ]; then
  # Derive it from the same clerk.env the rest of blaude reads.
  if [ -f "$HOME/.jcode/clerk.env" ]; then
    FAPI="$(grep -E '^CLERK_FRONTEND_API=' "$HOME/.jcode/clerk.env" | head -1 | cut -d= -f2- | tr -d '"'"'"' ')"
    # clerk.env stores the host WITHOUT a scheme (the runtime prepends https
    # too), and a scheme-less URL fetches a Cloudflare redirect page instead of
    # the keys — which would look like "Clerk sent nonsense" at startup.
    case "$FAPI" in http://*|https://*) ;; ?*) FAPI="https://$FAPI" ;; esac
    [ -n "$FAPI" ] && CLERK_JWKS_URL="${FAPI%/}/.well-known/jwks.json"
  fi
fi
[ -n "$CLERK_JWKS_URL" ] || {
  echo "CLERK_JWKS_URL is not set and could not be derived from ~/.jcode/clerk.env."
  echo "The service refuses every request without it, so it will not be deployed blind."
  exit 1; }
echo "   $CLERK_JWKS_URL"

say "build + deploy"
gcloud run deploy "$SERVICE" \
  --project "$PROJECT" \
  --region "$REGION" \
  --source . \
  --service-account "$SA_EMAIL" \
  --set-env-vars "CLERK_JWKS_URL=${CLERK_JWKS_URL}" \
  --set-secrets "/secrets/ssh/key=${SSH_SECRET}:latest" \
  --allow-unauthenticated \
  --timeout 900 \
  --cpu 1 --memory 1Gi \
  --max-instances 3 \
  --quiet

URL="$(gcloud run services describe "$SERVICE" --project "$PROJECT" --region "$REGION" \
        --format 'value(status.url)')"

say "done"
echo "   $URL"
echo
echo "Point the app's runtime at it:"
echo "   BLAUDE_PROVISION_API=$URL"
echo
echo "Check it:"
echo "   curl -s $URL/healthz"

# --allow-unauthenticated is deliberate and is NOT open access: the service
# verifies a Clerk session token on every route itself. Cloud Run's own IAM
# check would need a Google identity, which a blaude user does not have.
