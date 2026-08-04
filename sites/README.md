# Example sites

Run the supervisor from the repository root:

```sh
cargo run -- --root ./sites --listen 127.0.0.1:8080
```

Then try the examples by selecting a domain with the `Host` header:

```sh
curl -H 'Host: static.localhost' http://127.0.0.1:8080/
curl -H 'Host: shell.localhost' http://127.0.0.1:8080/hello?name=Eloi
curl -H 'Host: flask.localhost' http://127.0.0.1:8080/
curl -H 'Host: uvicorn.localhost' http://127.0.0.1:8080/
curl -H 'Host: rust.localhost' http://127.0.0.1:8080/
```

The static and shell examples have no external dependencies. The Flask and Uvicorn examples create an isolated `.venv` and install their `requirements.txt` on first use, so they require Python, `venv`, `pip`, and internet/package-index access at preparation time.

These folders are examples of the generic manifest contract. Flask and FastAPI receive no special treatment from `multi-server`; their JSON files merely describe commands that start HTTP servers on the dynamically assigned `${PORT}`.

## Extended behavior examples

| Domain | Demonstrates |
| --- | --- |
| `static.localhost` | Streaming files, `ETag`, `HEAD`, and byte ranges |
| `websocket.localhost` | WebSocket upgrade plus HTTP readiness |
| `limited.localhost` | Request-body, stdio-output, and concurrency limits |
| `process-group.localhost` | Graceful shutdown of an HTTP process and a child worker |
| `namespace-a.localhost` / `namespace-b.localhost` | Two isolated sites both listening on internal port `8080` |
| `health.localhost` | Delayed HTTP readiness returning `204` |
| `activity.localhost` | Only `/hls/` and `/publish/` renew the idle timeout |
| `rust.localhost` | Building and executing a dependency-free Rust HTTP binary |

Test a static byte range and conditional cache response:

```sh
curl -i -H 'Host: static.localhost' -H 'Range: bytes=10-19' \
  http://127.0.0.1:8080/media.txt
curl -I -H 'Host: static.localhost' http://127.0.0.1:8080/media.txt
```

Test the small request-body limit and HTTP readiness examples:

```sh
curl -i -H 'Host: limited.localhost' --data 'small body' http://127.0.0.1:8080/
curl -i -H 'Host: limited.localhost' --data 'this body is deliberately larger than thirty-two bytes' http://127.0.0.1:8080/
curl -H 'Host: health.localhost' http://127.0.0.1:8080/
```

With `websocat` installed, test the WebSocket echo endpoint:

```sh
websocat -H='Host: websocket.localhost' ws://127.0.0.1:8080/ws
```

The namespace pair requires `iproute2` and root or `CAP_NET_ADMIN`:

```sh
apk add --no-cache iproute2
curl -H 'Host: namespace-a.localhost' http://127.0.0.1:8080/
curl -H 'Host: namespace-b.localhost' http://127.0.0.1:8080/
```

Both responses report internal port `8080`, but each process runs in an independent namespace. Namespace examples do not configure outbound NAT; they only demonstrate isolated inbound HTTP routing.
