#!/bin/sh
set -eu
sleep 1
body=$(cat)
printf 'Content-Type: text/plain; charset=utf-8\r\n\r\n'
printf 'body=%s\n' "$body"
