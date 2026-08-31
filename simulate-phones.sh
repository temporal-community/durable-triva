#!/bin/sh
set -eu

count="${1:-100}"
exec cargo +stable run -p temporal-trivia-web --bin simulate-phones -- "$count"
