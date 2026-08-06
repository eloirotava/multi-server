<?php

header('Content-Type: application/json; charset=utf-8');

echo json_encode([
    'message' => 'FastCGI e front controller funcionando',
    'host' => $_SERVER['HTTP_HOST'] ?? null,
    'request_uri' => $_SERVER['REQUEST_URI'] ?? null,
    'script_filename' => $_SERVER['SCRIPT_FILENAME'] ?? null,
], JSON_PRETTY_PRINT | JSON_UNESCAPED_UNICODE);
