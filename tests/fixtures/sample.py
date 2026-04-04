"""Sample Python module for testing."""

from pathlib import Path


class Config:
    """Configuration holder."""

    DEFAULT_TIMEOUT = 30

    def __init__(self, path: str):
        """Initialize config."""
        self.path = Path(path)
        self.timeout = self.DEFAULT_TIMEOUT

    def load(self) -> dict:
        """Load configuration from path.

        Returns:
            Configuration dictionary.

        """
        if not self.path.exists():
            return {}
        return {"path": str(self.path)}


def create_config(path: str) -> Config:
    """Create a new Config instance.

    Returns:
        A Config object.

    """
    return Config(path)
