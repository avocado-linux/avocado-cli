"""SDK clean command implementation."""

import os
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_success, print_info, print_error
from avocado.utils.config import load_config
from avocado.utils.target import resolve_target, get_target_from_config


class SdkCleanCommand(BaseCommand):
    """Implementation of the 'sdk clean' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk clean command's subparser."""
        parser = subparsers.add_parser(
            "clean", help="Remove the SDK directory ($AVOCADO_SDK_PREFIX)"
        )

        parser.add_argument(
            "-c",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "-v", "--verbose", action="store_true", help="Enable verbose output"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk clean command."""
        config_path = args.config
        verbose = args.verbose

        # Load the configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image and target from configuration
        container_image = config.get("sdk", {}).get("image")
        if not container_image:
            print_error("No container image specified in config under 'sdk.image'")
            return False

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target = resolve_target(
            cli_target=args.resolved_target, config_target=config_target
        )
        if not target:
            print_error(
                "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'."
            )
            return False

        # Create container helper
        container_helper = SdkContainer()

        # Remove the directory using container helper
        if verbose:
            print_info(f"Removing SDK directory: $AVOCADO_SDK_PREFIX")

        remove_command = "rm -rf $AVOCADO_SDK_PREFIX"
        success = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=remove_command,
            verbose=verbose,
            source_environment=False,
        )

        if success:
            print_success(f"Successfully removed SDK directory.")
            return True
        else:
            print_error(f"Failed to remove SDK directory.")
            return False
