"""Output formatting utilities for Avocado CLI."""

import sys


class Colors:
    """ANSI color codes for terminal output."""

    RED = "\033[91m"
    GREEN = "\033[92m"
    YELLOW = "\033[93m"
    BLUE = "\033[94m"
    MAGENTA = "\033[95m"
    CYAN = "\033[96m"
    WHITE = "\033[97m"
    RESET = "\033[0m"


def print_error(message):
    """Print an error message with red [ERROR] prefix."""
    print(f"{Colors.RED}[ERROR]{Colors.RESET} {message}", file=sys.stderr)


def print_warning(message):
    """Print a warning message with yellow [WARNING] prefix."""
    print(f"{Colors.YELLOW}[WARNING]{Colors.RESET} {message}", file=sys.stderr)


def print_success(message):
    """Print a success message with green [SUCCESS] prefix."""
    print(f"{Colors.GREEN}[SUCCESS]{Colors.RESET} {message}")


def print_info(message):
    """Print an info message with blue [INFO] prefix."""
    print(f"{Colors.BLUE}[INFO]{Colors.RESET} {message}")


def print_debug(message):
    """Print a debug message with cyan [DEBUG] prefix."""
    print(f"{Colors.CYAN}[DEBUG]{Colors.RESET} {message}")
