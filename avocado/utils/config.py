"""Configuration utilities for Avocado CLI."""
import os
import tomlkit
from typing import Dict, Any, Optional, Tuple
from avocado.utils.output import print_error


class ConfigError(Exception):
    """Custom exception for configuration errors."""
    pass


class ConfigLoader:
    """Utility class for loading and validating configuration files."""

    @staticmethod
    def load_config(config_path: str) -> Tuple[Optional[Dict[Any, Any]], bool]:
        """
        Load and parse a TOML configuration file.

        Args:
            config_path: Path to the configuration file

        Returns:
            Tuple of (config_dict, success_flag)
            - config_dict: Parsed configuration or None if failed
            - success_flag: True if successful, False otherwise
        """
        # Check if configuration file exists
        if not os.path.exists(config_path):
            print_error(f"Configuration file '{config_path}' not found.")
            return None, False

        # Load and parse the configuration
        try:
            with open(config_path, "r") as f:
                config = tomlkit.parse(f.read())
            return config, True
        except Exception as e:
            print_error(f"loading configuration: {str(e)}.")
            return None, False


# Convenience function for common pattern
def load_config(config_path: str) -> Tuple[Optional[Dict[Any, Any]], bool]:
    """Convenience function to load a config file."""
    return ConfigLoader.load_config(config_path)
