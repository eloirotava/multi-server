# multi-server

`multi-server` maps HTTP host names to directories and uses a `site.json` manifest to decide how each site is served. Static sites consume no per-site processes; HTTP applications start on their first request and stop after an idle timeout.

## Run

```sh
cargo run -- --root ./sites --listen 127.0.0.1:8080
```

The repository includes ready-to-run examples under [`sites/`](sites/README.md), covering static and range requests, stdio scripts, Flask, FastAPI/Uvicorn, WebSockets, limits, health checks, process groups, activity policies, network namespaces, and a compiled Rust executable.

## Optimized Alpine amd64 build

The release profile enables full LTO, a single code-generation unit, abort-on-panic, optimization level 3, and symbol stripping. The repository also fixes the build target to `x86_64-unknown-linux-musl` with the static C runtime enabled. The resulting `multi-server` executable does not depend on the destination machine's musl, glibc, or other shared libraries.

On an amd64 Alpine machine, such as the environment used by PicoIDE, use a Rustup toolchain, install the native build tools, and compile with:

```sh
apk add --no-cache build-base
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The deployable executable is:

```text
target/x86_64-unknown-linux-musl/release/multi-server
```

Confirm its architecture, size, and linkage before copying it:

```sh
BIN=target/x86_64-unknown-linux-musl/release/multi-server
file "$BIN"
du -h "$BIN"
ldd "$BIN" 2>&1 || true
if readelf -l "$BIN" | grep -q 'INTERP'; then
  echo 'ERRO: o executável possui um interpretador dinâmico' >&2
  exit 1
fi
```

The default profile prioritizes runtime performance. If binary size matters more, override the optimization level for that build:

```sh
CARGO_PROFILE_RELEASE_OPT_LEVEL=z \
  cargo build --release --target x86_64-unknown-linux-musl
```

Do not set `-C target-cpu=native` when the binary may be moved to another amd64 machine: it can emit instructions unavailable on the destination CPU. If compilation and execution always happen on the same machine, an optional machine-specific build is:

```sh
RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=native" \
  cargo build --release --target x86_64-unknown-linux-musl
```

To install the optimized executable and keep the `sites` directory in a persistent location:

```sh
install -Dm755 \
  target/x86_64-unknown-linux-musl/release/multi-server \
  /usr/local/bin/multi-server
install -d /var/lib/multi-server/sites
multi-server --root /var/lib/multi-server/sites --listen 127.0.0.1:8080
```

Point `cloudflared` at `http://127.0.0.1:8080`. TLS terminates at the tunnel, so `multi-server` only needs to listen on local HTTP.

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

The generic HTTP mode also supports optional readiness, resource, activity, shutdown, and network policies:

```json
{
  "version": 1,
  "serve": {
    "mode": "http",
    "command": ["./server", "--port", "${PORT}"],
    "readiness": {
      "mode": "http",
      "path": "/health",
      "status": 200
    },
    "network": {
      "mode": "host"
    }
  },
  "lifecycle": {
    "idle_timeout_seconds": 60,
    "shutdown_grace_seconds": 10,
    "activity_paths": ["/api/", "/hls/"]
  },
  "limits": {
    "request_body_bytes": 67108864,
    "stdio_output_bytes": 16777216,
    "max_concurrent_requests": 64
  }
}
```

`activity_paths` controls which completed requests renew the idle deadline; every in-flight HTTP request or WebSocket still prevents shutdown. Readiness defaults to a TCP connection check. An HTTP readiness probe delays the first proxied request until the configured endpoint returns the expected status.

For two trusted applications that both require the same fixed port, Linux can place each one in its own native network namespace:

```json
{
  "serve": {
    "mode": "http",
    "command": ["./server"],
    "port": 8080,
    "network": { "mode": "namespace" }
  }
}
```

Namespace mode requires root or `CAP_NET_ADMIN`, the `ip` command from `iproute2`, and an application listening on `0.0.0.0` rather than only namespace-local `127.0.0.1`. The namespace and veth pair are created on demand and removed when the process stops.

`site.json` is watched for changes. For a deploy, copy the application files first and replace or touch `site.json` last. The supervisor lets active requests finish, stops the old process, runs `prepare` again, and starts the new version only when another request arrives. Other file changes are deliberately ignored so application data, uploads, caches, and generated media do not restart a site.

## Current scope

This version supports static files with streaming, conditional requests, and single byte ranges; per-request stdio/CGI programs; preparation commands; HTTP applications with dynamic or fixed ports; WebSocket upgrades; graceful process-group shutdown; health checks; request/concurrency limits; configurable activity; and optional native network namespaces. Proxied bodies are streamed rather than buffered in memory.

TLS remains intentionally outside the scope because the service is designed to listen behind a local Cloudflare Tunnel. Multiple byte ranges are rejected rather than encoded as multipart responses, and non-HTTP protocols such as RTMP, SRT, and raw TCP require a separate protocol-specific ingress process.
