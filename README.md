# multi-server

`multi-server` maps HTTP host names to directories and uses a `site.json` manifest to decide how each site is served. Static sites consume no per-site processes; HTTP applications start on their first request and stop after an idle timeout.

## Run

```sh
cargo run -- --root ./sites --listen 127.0.0.1:8080
```

Create `sites/eloi.rotava.com/site.json`:

```json
{
  "version": 1,
  "serve": {
    "mode": "http",
    "command": ["python3", "app.py"],
    "port_environment": "PORT"
  },
  "lifecycle": {
    "idle_timeout_seconds": 60
  }
}
```

The application must listen on `127.0.0.1:$PORT`. Then request it through the router:

```sh
curl -H 'Host: eloi.rotava.com' http://127.0.0.1:8080/
```

`${PORT}`, `${DOMAIN}`, and `${SITE_ROOT}` are expanded in HTTP command arguments, environment values, and the upstream host. This keeps the supervisor language-agnostic and also permits a generic container command to map a dynamically allocated host port to an application's hard-coded internal port:

```json
{
  "version": 1,
  "serve": {
    "mode": "http",
    "command": [
      "podman", "run", "--rm",
      "-p", "127.0.0.1:${PORT}:8080",
      "my-copied-application"
    ]
  }
}
```

Multiple applications may therefore use port `8080` internally while the supervisor routes each domain to a different dynamic host port. For an application already listening on a unique host port, set `"port": 8080`; dynamic allocation remains the default.

Static sites use a smaller manifest:

```json
{
  "version": 1,
  "serve": {
    "mode": "static",
    "root": "public",
    "index": ["index.html"]
  }
}
```

Exact domain directories take precedence. If no exact directory exists, parent-domain manifests with `"domains": { "subdomains": true }` may handle the request.

Commands that must run once before the first application start can be declared at the top level. A failed command prevents the site from starting:

```json
"prepare": [
  ["python3", "-m", "venv", ".venv"],
  [".venv/bin/pip", "install", "-r", "requirements.txt"]
]
```

Short-lived programs can use `"mode": "stdio"`. A process is started for every request, receives CGI-style request metadata in environment variables and the request body on standard input, and writes headers plus the response body to standard output.

`site.json` is watched for changes. For a deploy, copy the application files first and replace or touch `site.json` last. The supervisor lets active requests finish, stops the old process, runs `prepare` again, and starts the new version only when another request arrives. Other file changes are deliberately ignored so application data, uploads, caches, and generated media do not restart a site.

## Current scope

This version supports static files, per-request stdio/CGI programs, preparation commands, and HTTP applications with dynamically assigned ports. Proxied request and response bodies are streamed rather than buffered in memory. Long HTTP ingestion requests and repeated HLS segment requests count as activity, so an HTTP media process remains alive until both stop and its idle timeout expires.

Native network namespace creation is not implemented yet; container commands can already provide equivalent isolation without making the supervisor aware of a language or framework. TLS is intentionally outside the current scope because the service is designed to listen behind a local Cloudflare Tunnel. WebSocket upgrades, graceful process-group shutdown, and configurable activity probes remain planned follow-up capabilities.
