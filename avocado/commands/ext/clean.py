"""Extension clean command implementation."""

import os
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_error, print_success, print_info
from avocado.utils.target import resolve_target, get_target_from_config


class ExtCleanCommand(BaseCommand):
    """Implementation of the 'ext clean' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext clean command's subparser."""
        parser = subparsers.add_parser("clean", help="Clean an extension's sysroot")

        # Extension argument - can be positional or named
        parser.add_argument("extension", nargs="?", help="Name of the extension to clean")
        parser.add_argument(
            "-e",
            "--extension",
            dest="extension_named",
            help="Name of the extension to clean"
        )

        # Add optional arguments
        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "--verbose", "-v", action="store_true", help="Enable verbose output"
        )

        parser.add_argument(
            "--container-args",
            nargs="*",
            help="Additional arguments to pass to the container runtime (e.g., volume mounts, port mappings)",
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext clean command."""
        # Determine extension name from positional or named argument
        extension = getattr(args, 'extension_named', None) or args.extension
        if not extension:
            print_error("Extension name is required. Provide it positionally or via -e/--extension.")
            return False

        config_path = args.config
        verbose = args.verbose

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get runtime config
        runtime_config = config.get("runtime", {})

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target = resolve_target(
            cli_target=args.resolved_target, config_target=config_target
        )
        if not target:
            target = "qemux86-64"  # Default fallback

        # Get SDK config
        sdk_config = config.get("sdk", {})
        container_image = sdk_config.get("image", "avocadolinux/sdk:apollo-edge")

        # Initialize container helper
        container_helper = SdkContainer()

        # Check if extension sysroot exists
        sysroot_path = f"$AVOCADO_EXT_SYSROOTS/{extension}"
        check_cmd = f"test -d {sysroot_path}"

        if verbose:
            print_info(
                f"Checking if sysroot exists for extension '{
                       extension}'."
            )

        sysroot_exists = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=check_cmd,
            verbose=False,
            source_environment=False,
            container_args=getattr(args, 'container_args', None),
        )

        if not sysroot_exists:
            print_success(f"Sysroot for extension {extension} does not exist.")
            return True

        # Remove the extension sysroot
        print_info(f"Cleaning sysroot for extension '{extension}'.")

        remove_cmd = f"rm -rf {sysroot_path}"

        if verbose:
            print_info(f"Running command: {remove_cmd}")

        success = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=remove_cmd,
            verbose=verbose,
            source_environment=False,
            container_args=getattr(args, 'container_args', None),
        )

        if success:
            print_success(f"Cleaned sysroot for extension '{extension}'.")
            return True
        else:
            print_error(f"Failed to clean sysroot for extension '{extension}'.")
            return False
