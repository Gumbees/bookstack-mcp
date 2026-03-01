import httpx
import jwt
from jwt import PyJWKClient
from dataclasses import dataclass

from .config import Settings
from .token_store import TokenStore
from .bookstack import BookStackClient


@dataclass
class AuthenticatedUser:
    subject: str  # OIDC sub claim
    email: str
    name: str
    bookstack_user_id: int
    bookstack_token_id: str
    bookstack_token_secret: str


class AuthManager:
    def __init__(
        self,
        settings: Settings,
        token_store: TokenStore,
        bs_client: BookStackClient,
    ):
        self.settings = settings
        self.token_store = token_store
        self.bs_client = bs_client
        self._jwks_client: PyJWKClient | None = None
        self._oidc_config: dict | None = None

    async def get_oidc_config(self) -> dict:
        """Fetch and cache OIDC discovery document."""
        if self._oidc_config is None:
            async with httpx.AsyncClient() as client:
                resp = await client.get(
                    f"{self.settings.oidc_issuer}/.well-known/openid-configuration"
                )
                resp.raise_for_status()
                self._oidc_config = resp.json()
        return self._oidc_config

    def _get_jwks_client(self, jwks_uri: str) -> PyJWKClient:
        if self._jwks_client is None:
            self._jwks_client = PyJWKClient(jwks_uri)
        return self._jwks_client

    async def validate_token(self, access_token: str) -> dict:
        """Validate OAuth access token, return claims."""
        oidc_config = await self.get_oidc_config()
        jwks_client = self._get_jwks_client(oidc_config["jwks_uri"])
        signing_key = jwks_client.get_signing_key_from_jwt(access_token)

        claims = jwt.decode(
            access_token,
            signing_key.key,
            algorithms=["RS256", "ES256"],
            issuer=self.settings.oidc_issuer,
            audience=self.settings.oidc_audience or self.settings.oidc_client_id,
            options={"verify_exp": True},
        )
        return claims

    async def authenticate(self, access_token: str) -> AuthenticatedUser:
        """Full auth flow: validate OIDC token, resolve BookStack user, ensure API token."""
        claims = await self.validate_token(access_token)
        subject = claims["sub"]
        email = claims.get("email", "")
        name = claims.get("name", claims.get("preferred_username", ""))

        # Check if we already have a stored mapping
        stored = self.token_store.get_by_subject(subject)
        if stored:
            return AuthenticatedUser(
                subject=subject,
                email=email,
                name=name,
                bookstack_user_id=stored["bookstack_user_id"],
                bookstack_token_id=stored["bookstack_token_id"],
                bookstack_token_secret=stored["bookstack_token_secret"],
            )

        # Look up BookStack user by email (using admin token)
        bs_user = await self.bs_client.find_user_by_email(email)
        if not bs_user:
            raise PermissionError(f"No BookStack user found for {email}")

        # Create a new API token for this user
        token = await self.bs_client.create_api_token(
            user_id=bs_user["id"],
            name=f"mcp-{name}-auto",
        )

        # Store the mapping
        self.token_store.store(
            subject=subject,
            email=email,
            bookstack_user_id=bs_user["id"],
            bookstack_token_id=str(token["token_id"]),
            bookstack_token_secret=token["secret"],
        )

        return AuthenticatedUser(
            subject=subject,
            email=email,
            name=name,
            bookstack_user_id=bs_user["id"],
            bookstack_token_id=str(token["token_id"]),
            bookstack_token_secret=token["secret"],
        )
