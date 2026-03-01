import sqlite3
from pathlib import Path


class TokenStore:
    def __init__(self, db_path: str):
        Path(db_path).parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(db_path)
        self.conn.row_factory = sqlite3.Row
        self._init_db()

    def _init_db(self):
        self.conn.execute("""
            CREATE TABLE IF NOT EXISTS token_mappings (
                subject TEXT PRIMARY KEY,
                email TEXT NOT NULL,
                bookstack_user_id INTEGER NOT NULL,
                bookstack_token_id TEXT NOT NULL,
                bookstack_token_secret TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                last_used_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)
        self.conn.commit()

    def get_by_subject(self, subject: str) -> dict | None:
        row = self.conn.execute(
            "SELECT * FROM token_mappings WHERE subject = ?", (subject,)
        ).fetchone()
        if row:
            self.conn.execute(
                "UPDATE token_mappings SET last_used_at = CURRENT_TIMESTAMP WHERE subject = ?",
                (subject,),
            )
            self.conn.commit()
            return dict(row)
        return None

    def store(
        self,
        subject: str,
        email: str,
        bookstack_user_id: int,
        bookstack_token_id: str,
        bookstack_token_secret: str,
    ):
        self.conn.execute(
            """INSERT OR REPLACE INTO token_mappings
               (subject, email, bookstack_user_id, bookstack_token_id, bookstack_token_secret)
               VALUES (?, ?, ?, ?, ?)""",
            (subject, email, bookstack_user_id, bookstack_token_id, bookstack_token_secret),
        )
        self.conn.commit()

    def revoke(self, subject: str) -> bool:
        cursor = self.conn.execute(
            "DELETE FROM token_mappings WHERE subject = ?", (subject,)
        )
        self.conn.commit()
        return cursor.rowcount > 0
