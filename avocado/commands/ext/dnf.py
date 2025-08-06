from code import interact
from sys import intern
"""Extension dnf command implementation."""

import sys
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.config import load_config
from avocado.utils.output import print_error, print_info, print_success
from avocado.utils.target import resolve_target, get_target_from_config


class ExtDnfCommand(BaseCommand):
    """Implementation of the 'ext dnf' command."""

    def _do_extension_setup(
        self, config, extension, container_helper, container_image, target, verbose, container_args=None
    ):
        """Perform extension setup for DNF operations."""
        # Check if extension directory structure exists, create it if not
        check_cmd = f"test -d $AVOCADO_EXT_SYSROOTS/extensions/{extension}"
        repo_url = config.get("sdk", {}).get("repo_url")
        repo_release = config.get("sdk", {}).get("repo_release")

        print_info(repo_release)
        dir_exists = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=check_cmd,
            verbose=False,  # Don't show verbose output for the check
            source_environment=False,
            repo_url=repo_url,
            repo_release=repo_release,
            container_args=container_args,
        )
        if not dir_exists:
            print_info(f"Creating sysroot for extension '{extension}'.")
            setup_commands = [
                f"mkdir -p $AVOCADO_EXT_SYSROOTS/extensions/{
                    extension}/var/lib",
                f"cp -rf $AVOCADO_EXT_SYSROOTS/rootfs/var/lib/rpm $AVOCADO_EXT_SYSROOTS/extensions/{
                    extension}/var/lib",
            ]

            setup_success = container_helper.run_in_container(
                container_image=container_image,
                target=target,
                command=setup_commands,
                verbose=verbose,
                source_environment=False,
                repo_url=repo_url,
                repo_release=repo_release,
                container_args=container_args,
            )

            if setup_success:
                print_success(f"Created sysroot for extension '{extension}'.")
            else:
                print_error(
                    f"Failed to create sysroot for extension '{extension}'.")
                return False

        return True

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext dnf command's subparser."""
        parser = subparsers.add_parser(
            "dnf", help="Run DNF commands in an extension's context"
        )

        # Add common arguments
        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "-v", "--verbose", action="store_true", help="Enable verbose output"
        )

        parser.add_argument(
            "--container-args",
            help="Additional arguments to pass to the container runtime"
        )

        # Extension is now required as a flag
        parser.add_argument(
            "-e",
            "--extension",
            required=True,
            help="Extension name to operate on"
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
        """Execute the ext dnf command."""
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

        # Parse container args if provided
        container_args = None
        if args.container_args:
            import shlex
            container_args = shlex.split(args.container_args)

        config_path = args.config
        extension = args.extension

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

        # Check if extension exists in configuration
        if "ext" not in config or extension not in config["ext"]:
            print_error(
                f"Extension '{
                    extension}' not found in configuration."
            )
            return False

        # Do extension setup first
        if not self._do_extension_setup(
            config, extension, container_helper, container_image, target, args.verbose, container_args
        ):
            return False

        # Build dnf command with extension installroot
        installroot = f"$AVOCADO_EXT_SYSROOTS/{extension}"
        dnf_cmd = f"$DNF_SDK_HOST --installroot={
            installroot} {' '.join(command_args)}"

        # Run the DNF command using the container helper
        success = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=dnf_cmd,
            verbose=args.verbose,
            source_environment=False,
            use_entrypoint=True,
            repo_url=repo_url,
            repo_release=repo_release,
            container_args=container_args,
        )

        # Log the result
        if success:
            print_success(f"DNF command completed successfully.")
        else:
            print_error(f"DNF command failed.")

        return success

    def _print_help(self):
        """Print custom help message."""
        print(
            "usage: avocado ext dnf [-h] [-C CONFIG] [-v] -e EXTENSION -c COMMAND...")
        print()
        print("Execute DNF commands in an extension's context")
        print()
        print("options:")
        print("  -h, --help            show this help message and exit")
        print("  -C CONFIG, --config CONFIG")
        print(
            "                        Path to avocado.toml configuration file (default: avocado.toml)"
        )
        print("  -v, --verbose         Enable verbose output")
        print("  -e EXTENSION, --extension EXTENSION")
        print("                        Extension name to operate on (required)")
        print("  -c COMMAND, --command COMMAND")
        print("                        DNF command and arguments to execute (required)")
        print("  --container-args CONTAINER_ARGS")
        print(
            "                        Additional arguments to pass to the container runtime")
        print()
        print("Examples:")
        print("  avocado ext dnf -e myext -c repolist --enabled")
        print("  avocado ext dnf -e myext -c search python")
        print("  avocado ext dnf -e myext -c install vim")
        print("  avocado ext dnf -e myext -c --reinstall install gcc")
        print("  avocado ext dnf -e myext -c list installed")
        print("  avocado ext dnf --container-args '--network user-avocado' -e myext -c repolist")
