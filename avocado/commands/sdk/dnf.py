"""SDK DNF command implementation."""
import os
import sys
import tomlkit
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper
from avocado.utils.config import load_config
from avocado.utils.output import print_success, print_error


class SdkDnfCommand(BaseCommand):
    """Implementation of the 'sdk dnf' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk dnf command's subparser."""
        parser = subparsers.add_parser(
            "dnf",
            help="Run DNF commands in the SDK context"
        )

        # Add common arguments
        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        # Capture all remaining arguments after -- as dnf command
        parser.add_argument(
            "dnf_args",
            nargs="*",
            help="DNF command and arguments to execute (use -- to separate from SDK args)"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk dnf command."""
        # Add unknown args to dnf_args if they exist
        dnf_args = getattr(args, 'dnf_args', [])
        if unknown:
            dnf_args.extend(unknown)

        if not dnf_args:
            print(
                "Error: No DNF command specified. Use '--' to separate DNF arguments.", file=sys.stderr)
            if parser:
                parser.print_help()
            return False

        config_path = args.config

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image from configuration
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            print(
                "Error: No container image specified in config under 'sdk.image'", file=sys.stderr)
            return False

        # Get the target architecture from configuration
        target = config.get('runtime', {}).get('target')
        if not target:
            print(
                "Error: No target architecture specified in config under 'runtime.target'", file=sys.stderr)
            return False

        container_helper = SdkContainerHelper()

        # Build dnf command
        dnf_cmd = f"$DNF_SDK_HOST {' '.join(dnf_args)}"

        # Create the entrypoint script
        entrypoint_script = container_helper._create_entrypoint_script(
            target, source_environment=False)

        # Create the complete bash command
        bash_cmd = [
            "bash", "-c",
            entrypoint_script + f"\n# Execute DNF command\n{dnf_cmd}"
        ]

        # Run the command
        success = container_helper.runner.run_container_command(
            container_image=container_image,
            command=bash_cmd,
            target=target,
            interactive=True,
            tty=True
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
            "usage: avocado sdk dnf [-h] [-c CONFIG] -- <dnf_args>...")
        print()
        print("Run DNF commands in the SDK context")
        print()
        print("options:")
        print("  -h, --help            show this help message and exit")
        print("  -c CONFIG, --config CONFIG")
        print("                        Path to avocado.toml configuration file (default: avocado.toml)")
        print()
        print("dnf_args:")
        print("  Any DNF command and arguments to execute")
        print("  Note: Use '--' to separate DNF arguments from SDK arguments")
        print()
        print("Examples:")
        print("  avocado sdk dnf -- repolist --enabled")
        print("  avocado sdk dnf -- search python")
        print("  avocado sdk dnf -- install gcc make")
        print("  avocado sdk dnf -- --reinstall install cmake")
        print("  avocado sdk dnf -- list installed")
