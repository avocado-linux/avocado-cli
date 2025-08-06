"""SDK DNF command implementation."""

import os
import sys
import tomlkit


from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.config import load_config
from avocado.utils.output import print_success, print_error
from avocado.utils.target import resolve_target, get_target_from_config


class SdkDnfCommand(BaseCommand):
    """Implementation of the 'sdk dnf' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk dnf command's subparser."""
        parser = subparsers.add_parser(
            "dnf", help="Run DNF commands in the SDK context"
        )

        # Add common arguments
        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        # Command is now required as a flag
        parser.add_argument(
            "-c",
            "--command",
            nargs="*",
            required=True,
            help="DNF command and arguments to execute",
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk dnf command."""
        # Get command args from -c/--command flag
        command_args = getattr(args, "command", [])
        if unknown:
            command_args.extend(unknown)

        if not command_args:
            print(
                "Error: No DNF command specified.",
                file=sys.stderr,
            )
            if parser:
                parser.print_help()
            return False

        config_path = args.config

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image from configuration
        container_image = config.get("sdk", {}).get("image")
        if not container_image:
            print(
                "Error: No container image specified in config under 'sdk.image'",
                file=sys.stderr,
            )
            return False

        # Get repo_url from config, if it exists
        repo_url = config.get("sdk", {}).get("repo_url")
        repo_release = config.get("sdk", {}).get("repo_release")

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target = resolve_target(
            cli_target=args.resolved_target, config_target=config_target
        )
        if not target:
            print(
                "Error: No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.",
                file=sys.stderr,
            )
            return False

        container_helper = SdkContainer()

        # Build dnf command
        command = f"RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF {
            ' '.join(command_args)}"

        # Run the DNF command using the container helper
        success = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=[command],
            source_environment=False,
            use_entrypoint=True,
            repo_url=repo_url,
            repo_release=config.get("sdk", {}).get("repo_release"),
        )

        # Log the result
        if success:
            print_success(f"DNF command completed successfully.")
        else:
            print_error(f"DNF command failed.")

        return success

    def _print_help(self):
        """Print custom help message."""
        print("usage: avocado sdk dnf [-h] [-C CONFIG] -c COMMAND...")
        print()
        print("Run DNF commands in the SDK context")
        print()
        print("options:")
        print("  -h, --help            show this help message and exit")
        print("  -C CONFIG, --config CONFIG")
        print(
            "                        Path to avocado.toml configuration file (default: avocado.toml)"
        )
        print("  -c COMMAND, --command COMMAND")
        print("                        DNF command and arguments to execute (required)")
        print()
        print()
        print("Examples:")
        print("  avocado sdk dnf -c repolist --enabled")
        print("  avocado sdk dnf -c search python")
        print("  avocado sdk dnf -c install gcc make")
        print("  avocado sdk dnf -c --reinstall install cmake")
        print("  avocado sdk dnf -c list installed")
