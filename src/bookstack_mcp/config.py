from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    # BookStack instance URL
    bookstack_url: str  # e.g. https://docs.beesroadhouse.com

    # Server
    host: str = "0.0.0.0"
    port: int = 8080

    model_config = {"env_prefix": "BSMCP_"}
