from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import Response

app = FastAPI()


@app.get("/health")
async def health():
    return Response(status_code=204)


@app.websocket("/ws")
async def echo(websocket: WebSocket):
    await websocket.accept()
    try:
        while True:
            message = await websocket.receive_text()
            await websocket.send_text(f"echo: {message}")
    except WebSocketDisconnect:
        pass
