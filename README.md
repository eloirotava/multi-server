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

## Current scope

This version supports static files, per-request stdio/CGI programs, preparation commands, and HTTP applications with dynamically assigned ports. Long HTTP ingestion requests and repeated HLS segment requests count as activity, so an HTTP media process remains alive until both stop and its idle timeout expires.

Network namespaces for applications with colliding hard-coded ports are not implemented yet. TLS, WebSocket proxying, graceful process-group shutdown, automatic reload after file changes, and precise HLS publisher/viewer probes are also planned follow-up capabilities.
