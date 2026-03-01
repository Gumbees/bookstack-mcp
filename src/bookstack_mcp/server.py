import json
import re
from typing import Any

from mcp.server.fastmcp import FastMCP

from .auth import AuthenticatedUser
from .bookstack import BookStackClient

mcp = FastMCP(
    "BookStack MCP",
    description="MCP server for BookStack wiki",
)


# --- Search ---


@mcp.tool()
async def search_content(query: str, page: int = 1, count: int = 20) -> str:
    """Search across all BookStack content (pages, chapters, books, shelves).

    BookStack search supports operators:
    - {type:page} {type:chapter} {type:book} - filter by content type
    - [tag_name=value] - filter by tag
    - {in_name:term} - search only in names
    - {created_by:me} {updated_by:me} - filter by author
    - Wrap in quotes for exact match
    """
    from .transport import get_current_user_client

    client = get_current_user_client()
    results = await client.search(query, page=page, count=count)
    return _format_search_results(results)


# --- Shelves ---


@mcp.tool()
async def list_shelves(count: int = 50, offset: int = 0) -> str:
    """List all shelves. Shelves are the top-level organizational unit."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.list_shelves(count, offset))


@mcp.tool()
async def get_shelf(shelf_id: int) -> str:
    """Get a shelf by ID, including its books."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.get_shelf(shelf_id))


@mcp.tool()
async def create_shelf(name: str, description: str = "") -> str:
    """Create a new shelf."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.create_shelf(name, description))


@mcp.tool()
async def update_shelf(
    shelf_id: int, name: str | None = None, description: str | None = None
) -> str:
    """Update a shelf."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    kwargs = _filter_none(name=name, description=description)
    return _format_response(await client.update_shelf(shelf_id, **kwargs))


@mcp.tool()
async def delete_shelf(shelf_id: int) -> str:
    """Delete a shelf. This does NOT delete the books inside it."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    await client.delete_shelf(shelf_id)
    return f"Shelf {shelf_id} deleted."


# --- Books ---


@mcp.tool()
async def list_books(count: int = 50, offset: int = 0) -> str:
    """List all books."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.list_books(count, offset))


@mcp.tool()
async def get_book(book_id: int) -> str:
    """Get a book by ID, including its chapters and pages."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.get_book(book_id))


@mcp.tool()
async def create_book(name: str, description: str = "") -> str:
    """Create a new book. Optionally assign to a shelf after creation."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.create_book(name, description))


@mcp.tool()
async def update_book(
    book_id: int, name: str | None = None, description: str | None = None
) -> str:
    """Update a book."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    kwargs = _filter_none(name=name, description=description)
    return _format_response(await client.update_book(book_id, **kwargs))


@mcp.tool()
async def delete_book(book_id: int) -> str:
    """Delete a book and all its chapters/pages."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    await client.delete_book(book_id)
    return f"Book {book_id} deleted."


# --- Chapters ---


@mcp.tool()
async def get_chapter(chapter_id: int) -> str:
    """Get a chapter by ID, including its pages."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.get_chapter(chapter_id))


@mcp.tool()
async def create_chapter(book_id: int, name: str, description: str = "") -> str:
    """Create a new chapter within a book."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.create_chapter(book_id, name, description))


@mcp.tool()
async def update_chapter(
    chapter_id: int, name: str | None = None, description: str | None = None
) -> str:
    """Update a chapter."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    kwargs = _filter_none(name=name, description=description)
    return _format_response(await client.update_chapter(chapter_id, **kwargs))


@mcp.tool()
async def delete_chapter(chapter_id: int) -> str:
    """Delete a chapter. Pages inside become book-level pages."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    await client.delete_chapter(chapter_id)
    return f"Chapter {chapter_id} deleted."


# --- Pages ---


@mcp.tool()
async def get_page(page_id: int) -> str:
    """Get a page by ID, including full content."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(await client.get_page(page_id))


@mcp.tool()
async def create_page(
    name: str,
    markdown: str = "",
    html: str = "",
    book_id: int | None = None,
    chapter_id: int | None = None,
) -> str:
    """Create a new page. Must provide either book_id or chapter_id.
    Provide content as markdown (preferred) or html."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    return _format_response(
        await client.create_page(name, book_id, chapter_id, markdown, html)
    )


@mcp.tool()
async def update_page(
    page_id: int,
    name: str | None = None,
    markdown: str | None = None,
    html: str | None = None,
) -> str:
    """Update a page. Provide content as markdown (preferred) or html."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    kwargs = _filter_none(name=name, markdown=markdown, html=html)
    return _format_response(await client.update_page(page_id, **kwargs))


@mcp.tool()
async def delete_page(page_id: int) -> str:
    """Delete a page (moves to recycle bin)."""
    from .transport import get_current_user_client

    client = get_current_user_client()
    await client.delete_page(page_id)
    return f"Page {page_id} deleted."


# --- Token management ---


@mcp.tool()
async def revoke_my_token() -> str:
    """Revoke the current MCP session's BookStack API token.
    Next connection will create a fresh one."""
    from .transport import get_auth_manager, get_current_user

    user = get_current_user()
    auth = get_auth_manager()
    # Delete from BookStack
    admin_client = BookStackClient(
        auth.settings.bookstack_url,
        auth.settings.bookstack_admin_token_id,
        auth.settings.bookstack_admin_token_secret,
    )
    await admin_client.delete_api_token(
        user.bookstack_user_id, int(user.bookstack_token_id)
    )
    # Remove from local store
    auth.token_store.revoke(user.subject)
    return "Token revoked. Reconnect to generate a new one."


# --- Helpers ---


def _filter_none(**kwargs) -> dict[str, Any]:
    return {k: v for k, v in kwargs.items() if v is not None}


def _format_response(data: dict | list | None) -> str:
    if data is None:
        return "OK"
    return json.dumps(data, indent=2, default=str)


def _format_search_results(data: dict) -> str:
    results = data.get("data", [])
    total = data.get("total", 0)
    if not results:
        return "No results found."

    lines = [f"Found {total} results:\n"]
    for item in results:
        item_type = item.get("type", "unknown")
        lines.append(f"- [{item_type}] {item['name']} (id: {item['id']})")
        preview_html = item.get("preview_html")
        if preview_html:
            raw = (
                preview_html.get("content", "")
                if isinstance(preview_html, dict)
                else str(preview_html)
            )
            preview = re.sub(r"<[^>]+>", "", raw)
            lines.append(f"  Preview: {preview[:200]}")
        lines.append("")
    return "\n".join(lines)
