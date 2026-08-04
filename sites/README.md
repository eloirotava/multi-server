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
```

The static and shell examples have no external dependencies. The Flask and Uvicorn examples create an isolated `.venv` and install their `requirements.txt` on first use, so they require Python, `venv`, `pip`, and internet/package-index access at preparation time.

These folders are examples of the generic manifest contract. Flask and FastAPI receive no special treatment from `multi-server`; their JSON files merely describe commands that start HTTP servers on the dynamically assigned `${PORT}`.
