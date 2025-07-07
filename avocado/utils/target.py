"""Target resolution utilities for Avocado CLI."""
import os
from typing import Optional


def resolve_target(cli_target: Optional[str] = None, config_target: Optional[str] = None) -> Optional[str]:
    """
    Resolve the target architecture with proper precedence.

    Precedence order:
    1. CLI argument (--target, -t)
    2. Environment variable (AVOCADO_TARGET)
    3. Configuration file (future - currently not implemented)

    Args:
        cli_target: Target from CLI argument (highest priority)
        config_target: Target from configuration file (lowest priority, future use)

    Returns:
        Resolved target string or None if no target is specified
    """
    # First priority: CLI argument
    if cli_target is not None:
        return cli_target

    # Second priority: Environment variable
    env_target = os.environ.get('AVOCADO_TARGET')
    if env_target is not None:
        return env_target

    # Third priority: Configuration file (future implementation)
    # Currently omitted per requirements - waiting for config key location decision
    if config_target is not None:
        return config_target

    return None


def get_target_from_env() -> Optional[str]:
    """
    Get target from environment variable.

    Returns:
        Target from AVOCADO_TARGET environment variable or None
    """
    return os.environ.get('AVOCADO_TARGET')


def get_target_from_config(config: dict) -> Optional[str]:
    """
    Extract target from configuration file using new structure.

    Looks for target in [runtime.<name>] sections. If there's exactly one
    runtime configuration, uses its target value.

    Args:
        config: Parsed configuration dictionary

    Returns:
        Target from config or None if not found/ambiguous
    """
    if not config or 'runtime' not in config:
        return None

    runtime_section = config['runtime']
    if not isinstance(runtime_section, dict):
        return None

    # Find all runtime configurations (nested dictionaries)
    runtime_configs = {}
    for key, value in runtime_section.items():
        if isinstance(value, dict):
            runtime_configs[key] = value

    # If exactly one runtime configuration, use its target
    if len(runtime_configs) == 1:
        runtime_config = next(iter(runtime_configs.values()))
        return runtime_config.get('target')

    # If multiple or no runtime configurations, return None
    return None
