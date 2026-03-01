import httpx
from typing import Any


class BookStackClient:
    """BookStack API client. Operates with admin creds or per-user creds."""

    def __init__(self, base_url: str, token_id: str = "", token_secret: str = ""):
        self.base_url = base_url.rstrip("/")
        self.token_id = token_id
        self.token_secret = token_secret

    def _headers(self) -> dict:
        return {
            "Authorization": f"Token {self.token_id}:{self.token_secret}",
            "Content-Type": "application/json",
        }

    def with_credentials(self, token_id: str, token_secret: str) -> "BookStackClient":
        """Return a new client instance with different credentials."""
        return BookStackClient(self.base_url, token_id, token_secret)

    async def _request(self, method: str, path: str, **kwargs) -> Any:
        async with httpx.AsyncClient() as client:
            resp = await client.request(
                method,
                f"{self.base_url}/api/{path}",
                headers=self._headers(),
                **kwargs,
            )
            resp.raise_for_status()
            return resp.json() if resp.content else None

    # --- User management (admin) ---

    async def find_user_by_email(self, email: str) -> dict | None:
        data = await self._request("GET", "users", params={"filter[email]": email})
        users = data.get("data", [])
        return users[0] if users else None

    async def create_api_token(self, user_id: int, name: str) -> dict:
        return await self._request(
            "POST",
            f"users/{user_id}/api-tokens",
            json={"name": name, "expires_at": None},
        )

    async def delete_api_token(self, user_id: int, token_id: int) -> None:
        await self._request("DELETE", f"users/{user_id}/api-tokens/{token_id}")

    # --- Shelves ---

    async def list_shelves(self, count: int = 100, offset: int = 0) -> dict:
        return await self._request(
            "GET", "shelves", params={"count": count, "offset": offset}
        )

    async def get_shelf(self, shelf_id: int) -> dict:
        return await self._request("GET", f"shelves/{shelf_id}")

    async def create_shelf(self, name: str, description: str = "") -> dict:
        return await self._request(
            "POST", "shelves", json={"name": name, "description": description}
        )

    async def update_shelf(self, shelf_id: int, **kwargs) -> dict:
        return await self._request("PUT", f"shelves/{shelf_id}", json=kwargs)

    async def delete_shelf(self, shelf_id: int) -> None:
        await self._request("DELETE", f"shelves/{shelf_id}")

    # --- Books ---

    async def list_books(self, count: int = 100, offset: int = 0) -> dict:
        return await self._request(
            "GET", "books", params={"count": count, "offset": offset}
        )

    async def get_book(self, book_id: int) -> dict:
        return await self._request("GET", f"books/{book_id}")

    async def create_book(self, name: str, description: str = "", **kwargs) -> dict:
        return await self._request(
            "POST",
            "books",
            json={"name": name, "description": description, **kwargs},
        )

    async def update_book(self, book_id: int, **kwargs) -> dict:
        return await self._request("PUT", f"books/{book_id}", json=kwargs)

    async def delete_book(self, book_id: int) -> None:
        await self._request("DELETE", f"books/{book_id}")

    # --- Chapters ---

    async def list_chapters(self, count: int = 100, offset: int = 0) -> dict:
        return await self._request(
            "GET", "chapters", params={"count": count, "offset": offset}
        )

    async def get_chapter(self, chapter_id: int) -> dict:
        return await self._request("GET", f"chapters/{chapter_id}")

    async def create_chapter(
        self, book_id: int, name: str, description: str = ""
    ) -> dict:
        return await self._request(
            "POST",
            "chapters",
            json={"book_id": book_id, "name": name, "description": description},
        )

    async def update_chapter(self, chapter_id: int, **kwargs) -> dict:
        return await self._request("PUT", f"chapters/{chapter_id}", json=kwargs)

    async def delete_chapter(self, chapter_id: int) -> None:
        await self._request("DELETE", f"chapters/{chapter_id}")

    # --- Pages ---

    async def list_pages(self, count: int = 100, offset: int = 0) -> dict:
        return await self._request(
            "GET", "pages", params={"count": count, "offset": offset}
        )

    async def get_page(self, page_id: int) -> dict:
        return await self._request("GET", f"pages/{page_id}")

    async def create_page(
        self,
        name: str,
        book_id: int | None = None,
        chapter_id: int | None = None,
        markdown: str = "",
        html: str = "",
    ) -> dict:
        payload: dict[str, Any] = {"name": name}
        if chapter_id:
            payload["chapter_id"] = chapter_id
        elif book_id:
            payload["book_id"] = book_id
        if markdown:
            payload["markdown"] = markdown
        elif html:
            payload["html"] = html
        return await self._request("POST", "pages", json=payload)

    async def update_page(self, page_id: int, **kwargs) -> dict:
        return await self._request("PUT", f"pages/{page_id}", json=kwargs)

    async def delete_page(self, page_id: int) -> None:
        await self._request("DELETE", f"pages/{page_id}")

    # --- Search ---

    async def search(self, query: str, page: int = 1, count: int = 20) -> dict:
        return await self._request(
            "GET", "search", params={"query": query, "page": page, "count": count}
        )

    # --- Attachments ---

    async def list_attachments(self) -> dict:
        return await self._request("GET", "attachments")

    async def get_attachment(self, attachment_id: int) -> dict:
        return await self._request("GET", f"attachments/{attachment_id}")
