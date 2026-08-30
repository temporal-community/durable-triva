#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -n "${TEMPORAL_ENV_FILE:-}" ]; then
    if [ ! -f "$TEMPORAL_ENV_FILE" ]; then
        echo "TEMPORAL_ENV_FILE points to missing file $TEMPORAL_ENV_FILE" >&2
        exit 1
    fi
    settings="$TEMPORAL_ENV_FILE"
else
    settings="$project_dir/.env"
    if [ ! -f "$settings" ]; then
        settings="$project_dir/.env.temporal"
    fi
fi
if [ -f "$settings" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$settings"
    set +a
fi

: "${TEMPORAL_ADDRESS:?missing TEMPORAL_ADDRESS}"
: "${TEMPORAL_NAMESPACE:?missing TEMPORAL_NAMESPACE}"
: "${TEMPORAL_API_KEY:?missing TEMPORAL_API_KEY}"

address=${TEMPORAL_ADDRESS#https://}
existing=$(temporal operator search-attribute list \
    --address "$address" \
    --namespace "$TEMPORAL_NAMESPACE" \
    --tls)

register() {
    name=$1
    type=$2
    if printf '%s\n' "$existing" | grep -q "$name"; then
        echo "$name already registered"
        return
    fi
    temporal operator search-attribute create \
        --address "$address" \
        --namespace "$TEMPORAL_NAMESPACE" \
        --tls \
        --name "$name" \
        --type "$type"
}

register TriviaGameStatus Keyword
register TriviaBadgeCount Int
register TriviaReassignments Int
register TriviaWinner Keyword
register TriviaRustSdk Bool
