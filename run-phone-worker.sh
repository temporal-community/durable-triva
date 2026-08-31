#!/bin/sh
set -eu

exec cargo +stable run -p temporal-trivia-web --bin phone_worker
