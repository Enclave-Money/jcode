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
ZONE="${BLAUDE_PROVISION_ZONE:-${REGION}-a}"
SERVICE="${BLAUDE_PROVISION_SERVICE:-blaude-provision-api}"
SA="blaude-provision"
SA_EMAIL="${SA}@${PROJECT}.iam.gserviceaccount.com"
SSH_SECRET="blaude-provision-ssh-key"
RELAY_SECRET="blaude-directory-relay-key"
ALLOWED_EMAILS="${BLAUDE_PROVISION_ALLOWED_EMAILS:-}"
AUTHORIZED_PARTIES="${BLAUDE_CLERK_AUTHORIZED_PARTIES:-}"
ALLOW_ALL="${BLAUDE_PROVISION_ALLOW_ALL:-0}"

say() { printf '\n== %s\n' "$*"; }

command -v gcloud >/dev/null || { echo "gcloud is not installed"; exit 1; }
gcloud auth print-access-token >/dev/null 2>&1 || {
  echo "gcloud is not signed in. Run: gcloud auth login"; exit 1; }

# Each accepted account can allocate a continuously billed e2-standard-4 VM.
# An omitted allowlist must never turn an ordinary redeploy into public compute
# access. A genuinely public deployment has to say so explicitly.
if [ -z "$ALLOWED_EMAILS" ] && [ "$ALLOW_ALL" != "1" ]; then
  echo "BLAUDE_PROVISION_ALLOWED_EMAILS is empty; refusing to deploy open VM creation."
  echo "Set a comma-separated allowlist, or explicitly set BLAUDE_PROVISION_ALLOW_ALL=1."
  exit 1
fi

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

# Clerk's backend credential stays in this service so it can send invites.
#
# Team servers receive only a signed, team-scoped relay capability. Copying
# this file onto every VM would hand a compromised team backend-wide control
# over every account in the Clerk instance.
say "clerk.env secret"
CLERK_SECRET="blaude-clerk-env"
if ! gcloud secrets describe "$CLERK_SECRET" --project "$PROJECT" >/dev/null 2>&1; then
  [ -f "$HOME/.jcode/clerk.env" ] || {
    echo "~/.jcode/clerk.env is missing; cannot seed the invite credential."; exit 1; }
  gcloud secrets create "$CLERK_SECRET" --project "$PROJECT" \
    --data-file="$HOME/.jcode/clerk.env" --quiet
  echo "   created $CLERK_SECRET"
else
  echo "   $CLERK_SECRET already exists"
fi

# A stable HMAC key for team-scoped directory relay capabilities. The key is
# mounted only in Cloud Run; each VM receives a signed capability, never this
# signing key or the Clerk backend key.
say "directory relay signing key"
if ! gcloud secrets describe "$RELAY_SECRET" --project "$PROJECT" >/dev/null 2>&1; then
  relay_key_file="$(mktemp)"
  openssl rand -hex 32 > "$relay_key_file"
  gcloud secrets create "$RELAY_SECRET" --project "$PROJECT" \
    --data-file="$relay_key_file" --quiet
  rm -f "$relay_key_file"
  echo "   created $RELAY_SECRET"
else
  echo "   $RELAY_SECRET already exists"
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
if [ -n "$ALLOWED_EMAILS" ]; then
  echo "   provisioning allowlist: $ALLOWED_EMAILS"
else
  echo "   provisioning allowlist is unset; every account admitted by this Clerk instance may create teams"
fi

# Build the image OURSELVES.
#
# `gcloud run deploy --source .` only reads a Dockerfile at the root of the
# source directory. With ours one level down it fell through to buildpacks,
# decided this repo was Python, and failed asking for a main.py — which reads
# nothing like "your Dockerfile was ignored".
say "artifact registry"
REPO="blaude"
IMAGE="${REGION}-docker.pkg.dev/${PROJECT}/${REPO}/${SERVICE}:$(date +%Y%m%d-%H%M%S)"
gcloud artifacts repositories describe "$REPO" \
  --project "$PROJECT" --location "$REGION" >/dev/null 2>&1 || \
  gcloud artifacts repositories create "$REPO" \
    --project "$PROJECT" --location "$REGION" \
    --repository-format docker \
    --description "blaude service images" --quiet

say "build"
gcloud builds submit \
  --project "$PROJECT" \
  --region "$REGION" \
  --config deploy/provision-api/cloudbuild.yaml \
  --substitutions "_IMAGE=${IMAGE}" \
  --quiet

say "deploy"
gcloud run deploy "$SERVICE" \
  --project "$PROJECT" \
  --region "$REGION" \
  --image "$IMAGE" \
  --service-account "$SA_EMAIL" \
  --set-env-vars "^|^CLERK_JWKS_URL=${CLERK_JWKS_URL}|BLAUDE_PROJECT=${PROJECT}|BLAUDE_ZONE=${ZONE}|BLAUDE_PROVISION_ALLOWED_EMAILS=${ALLOWED_EMAILS}|BLAUDE_CLERK_AUTHORIZED_PARTIES=${AUTHORIZED_PARTIES}" \
  --set-secrets "/secrets/ssh/key=${SSH_SECRET}:latest,/secrets/clerk/env=${CLERK_SECRET}:latest,/secrets/relay/key=${RELAY_SECRET}:latest" \
  --allow-unauthenticated \
  --timeout 900 \
  --cpu 1 --memory 1Gi \
  --no-cpu-throttling \
  --max-instances 1 \
  --quiet

# Those last two are load-bearing, not tuning:
#
# no-cpu-throttling — a create runs as a BACKGROUND task after the POST has
# answered 202, and Cloud Run's default allocates CPU only while a request is
# in flight. Throttled, the build would inch forward only during the client's
# 2-second polls and stall the rest of the time.
#
# max-instances 1 — jobs live in the process's memory, so a poll that lands on
# a second instance finds nothing and reads as "job vanished". One instance
# makes every poll land where the job is. If team creation ever outgrows one
# instance, the job store moves to something shared; the cap does not just get
# raised.

URL="$(gcloud run services describe "$SERVICE" --project "$PROJECT" --region "$REGION" \
        --format 'value(status.url)')"

say "done"
echo "   $URL"
echo
echo "Point the app's runtime at it:"
echo "   BLAUDE_PROVISION_API=$URL"
echo
echo "Check it:"
echo "   curl -s $URL/v1/health"

# --allow-unauthenticated is deliberate and is NOT open access: the service
# verifies a Clerk session token on every route itself. Cloud Run's own IAM
# check would need a Google identity, which a blaude user does not have.
