"""HTTP SSE transport with BookStack API token auth.

Users authenticate by passing their BookStack API token as a Bearer token
in the format: Bearer <token_id>:<token_secret>

The server validates the token against BookStack and all subsequent API calls
use that user's credentials and permissions.
"""
import contextvars

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route
from mcp.server.sse import SseServerTransport

from .bookstack import BookStackClient
from .config import Settings
from .server import mcp

# Per-request client, set by auth middleware
_current_client: contextvars.ContextVar[BookStackClient] = contextvars.ContextVar(
    "current_client"
)


def get_current_client() -> BookStackClient:
    return _current_client.get()


class BookStackAuthMiddleware:
    """Validates BookStack API tokens on MCP endpoints.

    Clients pass their BookStack API token as:
        Authorization: Bearer <token_id>:<token_secret>

    The middleware validates by calling /api/users (self) and sets up
    a per-request BookStack client with those credentials.
    """

    def __init__(self, app, bookstack_url: str):
        self.app = app
        self.bookstack_url = bookstack_url

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        request = Request(scope, receive)
        path = request.url.path

        # Health endpoint — unauthenticated
        if path == "/health":
            await self.app(scope, receive, send)
            return

        # MCP endpoints require BookStack API token
        if path.startswith("/mcp"):
            auth_header = request.headers.get("authorization", "")
            if not auth_header.startswith("Bearer "):
                response = JSONResponse(
                    {"error": "unauthorized", "hint": "Bearer <token_id>:<token_secret>"},
                    status_code=401,
                )
                await response(scope, receive, send)
                return

            token = auth_header.removeprefix("Bearer ").strip()
            if ":" not in token:
                response = JSONResponse(
                    {"error": "invalid token format", "hint": "Expected <token_id>:<token_secret>"},
                    status_code=401,
                )
                await response(scope, receive, send)
                return

            token_id, token_secret = token.split(":", 1)
            bs_client = BookStackClient(self.bookstack_url, token_id, token_secret)

            # Validate by making a lightweight API call
            try:
                await bs_client.get_current_user()
            except Exception:
                response = JSONResponse(
                    {"error": "invalid BookStack credentials"},
                    status_code=403,
                )
                await response(scope, receive, send)
                return

            _current_client.set(bs_client)

        await self.app(scope, receive, send)


def create_app() -> Starlette:
    settings = Settings()
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
        await sse_transport.handle_post_message(
            request.scope, request.receive, request._send
        )

    async def handle_health(request: Request):
        return JSONResponse({"status": "ok"})

    inner_app = Starlette(
        routes=[
            Route("/mcp/sse", endpoint=handle_sse),
            Route("/mcp/messages/", endpoint=handle_messages, methods=["POST"]),
            Route("/health", endpoint=handle_health),
        ],
    )

    app = BookStackAuthMiddleware(inner_app, bookstack_url=settings.bookstack_url)
    return app


def main():
    import uvicorn

    settings = Settings()
    app = create_app()
    uvicorn.run(app, host=settings.host, port=settings.port)


if __name__ == "__main__":
    main()
