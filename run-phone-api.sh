#!/bin/sh
set -eu

export PHONE_COOKIE_SECURE="${PHONE_COOKIE_SECURE:-0}"
export PORT="${PORT:-8080}"

exec cargo +stable run -p temporal-trivia-web --bin phone_api
