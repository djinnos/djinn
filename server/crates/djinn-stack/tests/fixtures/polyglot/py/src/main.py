from fastapi import FastAPI

app = FastAPI()


@app.get("/")
def root() -> dict:
    return {"hello": "world"}
