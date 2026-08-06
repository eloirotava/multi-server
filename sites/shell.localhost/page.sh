#!/bin/sh
set -eu

printf 'Status: 200 OK\r\n'
printf 'Content-Type: text/html; charset=utf-8\r\n'
printf '\r\n'
printf '<!doctype html><html lang="pt-BR"><body>'
printf '<h1>Script shell funcionando</h1>'
printf '<p>Método: %s</p>' "${REQUEST_METHOD:-desconhecido}"
printf '<p>Caminho: %s</p>' "${REQUEST_PATH:-/}"
printf '<p>Query: %s</p>' "${QUERY_STRING:-}"
printf '</body></html>'
