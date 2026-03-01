"""HTTP SSE transport with OAuth 2.1 middleware."""
import contextvars

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route
from mcp.server.sse import SseServerTransport

from .auth import AuthManager, AuthenticatedUser
from .bookstack import BookStackClient
from .config import Settings
from .token_store import TokenStore
from .server import mcp

# Context vars for per-request auth state
_current_user: contextvars.ContextVar[AuthenticatedUser] = contextvars.ContextVar(
    "current_user"
)
_current_client: contextvars.ContextVar[BookStackClient] = contextvars.ContextVar(
    "current_client"
)
_auth_manager_var: contextvars.ContextVar[AuthManager] = contextvars.ContextVar(
    "auth_manager"
)


def get_current_user() -> AuthenticatedUser:
    return _current_user.get()


def get_current_user_client() -> BookStackClient:
    return _current_client.get()


def get_auth_manager() -> AuthManager:
    return _auth_manager_var.get()


class OAuthAuthMiddleware:
    """Validates Bearer tokens on MCP endpoints, passes through OAuth metadata requests."""

    def __init__(self, app, auth: AuthManager, settings: Settings):
        self.app = app
        self.auth = auth
        self.settings = settings

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        request = Request(scope, receive)
        path = request.url.path

        # OAuth metadata endpoint (unauthenticated)
        if path == "/.well-known/oauth-authorization-server":
            oidc_config = await self.auth.get_oidc_config()
            response = JSONResponse(
                {
                    "issuer": self.settings.base_url,
                    "authorization_endpoint": oidc_config.get(
                        "authorization_endpoint",
                        f"{self.settings.oidc_issuer}/authorize/",
                    ),
                    "token_endpoint": oidc_config.get(
                        "token_endpoint",
                        f"{self.settings.oidc_issuer}/token/",
                    ),
                    "registration_endpoint": None,
                    "response_types_supported": ["code"],
                    "grant_types_supported": [
                        "authorization_code",
                        "refresh_token",
                    ],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": [
                        "client_secret_post",
                        "client_secret_basic",
                    ],
                }
            )
            await response(scope, receive, send)
            return

        # Health endpoint (unauthenticated)
        if path == "/health":
            response = JSONResponse({"status": "ok"})
            await response(scope, receive, send)
            return

        # MCP endpoints require auth
        if path.startswith("/mcp"):
            auth_header = request.headers.get("authorization", "")
            if not auth_header.startswith("Bearer "):
                response = JSONResponse(
                    {"error": "unauthorized"},
                    status_code=401,
                    headers={
                        "WWW-Authenticate": (
                            f'Bearer resource_metadata='
                            f'"{self.settings.base_url}/.well-known/oauth-authorization-server"'
                        )
                    },
                )
                await response(scope, receive, send)
                return

            token = auth_header.removeprefix("Bearer ")
            try:
                user = await self.auth.authenticate(token)
                _current_user.set(user)
                _current_client.set(
                    BookStackClient(
                        self.settings.bookstack_url,
                        user.bookstack_token_id,
                        user.bookstack_token_secret,
                    )
                )
                _auth_manager_var.set(self.auth)
            except Exception as e:
                response = JSONResponse({"error": str(e)}, status_code=403)
                await response(scope, receive, send)
                return

        await self.app(scope, receive, send)


def create_app() -> Starlette:
    settings = Settings()
    token_store = TokenStore(settings.token_db_path)
    admin_client = BookStackClient(
        settings.bookstack_url,
        settings.bookstack_admin_token_id,
        settings.bookstack_admin_token_secret,
    )
    auth = AuthManager(settings, token_store, admin_client)

    sse_transport = SseServerTransport("/mcp/messages/")

    async def handle_sse(request: Request):
        async with sse_transport.connect_sse(
            request.scope, request.receive, request._send
        ) as streams:
            await mcp._mcp_server.run(
                streams[0],
                streams[1],
                mcp._mcp_server.create_initialization_options(),
            )

    async def handle_messages(request: Request):
        await sse_transport.handle_post_message(request.scope, request.receive, request._send)

    # Build the inner app with routes
    inner_app = Starlette(
        routes=[
            Route("/mcp/sse", endpoint=handle_sse),
            Route("/mcp/messages/", endpoint=handle_messages, methods=["POST"]),
            Route(
                "/.well-known/oauth-authorization-server",
                endpoint=lambda r: JSONResponse({}),  # handled by middleware
            ),
            Route("/health", endpoint=lambda r: JSONResponse({"status": "ok"})),
        ],
    )

    # Wrap with auth middleware
    app = OAuthAuthMiddleware(inner_app, auth=auth, settings=settings)

    return app


def main():
    import uvicorn

    settings = Settings()
    app = create_app()
    uvicorn.run(app, host=settings.host, port=settings.port)


if __name__ == "__main__":
    main()
