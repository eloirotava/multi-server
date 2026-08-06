import os

from flask import Flask, jsonify, request

app = Flask(__name__)


@app.get("/")
def index():
    return jsonify(
        framework="Flask",
        message="Aplicação iniciada sob demanda",
        path=request.path,
    )


if __name__ == "__main__":
    app.run(host="127.0.0.1", port=int(os.environ["PORT"]))
