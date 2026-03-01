from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    # BookStack
    bookstack_url: str  # e.g. https://docs.beesroadhouse.com
    bookstack_admin_token_id: str
    bookstack_admin_token_secret: str

    # OIDC (Authentik)
    oidc_issuer: str  # e.g. https://auth.beesroadhouse.com/application/o/bookstack-mcp
    oidc_client_id: str
    oidc_client_secret: str
    oidc_audience: str | None = None

    # Server
    host: str = "0.0.0.0"
    port: int = 8080
    base_url: str  # e.g. https://bookstack-mcp.beesroadhouse.com

    # Token store
    token_db_path: str = "data/tokens.db"

    model_config = {"env_prefix": "BSMCP_"}
