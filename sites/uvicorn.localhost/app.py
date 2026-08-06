from fastapi import FastAPI

app = FastAPI()


@app.get("/")
async def index():
    return {
        "framework": "FastAPI/Uvicorn",
        "message": "Aplicação iniciada sob demanda",
    }
