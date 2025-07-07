"""SDK run subcommand implementation."""
import os
import sys
import tomlkit
from pathlib import Path
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_error, print_success
from avocado.utils.target import resolve_target, get_target_from_config


class SdkRunCommand(BaseCommand):
    """Implementation of the 'sdk run' subcommand."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk run subcommand's subparser."""
        run_parser = subparsers.add_parser(
            "run", help="Create and run an SDK container")
        run_parser.add_argument("-c", "--config", default="avocado.toml",
                                help="Path to avocado.toml configuration file (default: avocado.toml)")
        run_parser.add_argument("--name", type=str, default=None,
                                help="Assign a name to the container")
        run_parser.add_argument("--detach", "-d", action="store_true",
                                help="Run container in background and print container ID")
        run_parser.add_argument("--rm", action="store_true",
                                help="Automatically remove the container when it exits")
        run_parser.add_argument("--interactive", "-i", action="store_true",
                                help="Drop into interactive shell in container. If specified, 'command' is ignored.")
        run_parser.add_argument("-v", "--verbose", action="store_true",
                                help="Enable verbose output")
        run_parser.add_argument("command", nargs="*",
                                help="Command and arguments to run in container. Used if --interactive is not specified.")
        return run_parser

    def _load_config(self, config_path):
        """Load and parse the TOML config file."""
        abs_config_path = Path(config_path).resolve()

        try:
            with open(abs_config_path, 'r') as f:
                config = tomlkit.load(f)
            return config
        except FileNotFoundError:
            print_error(f"Config file not found at {
                abs_config_path}")
            raise
        except Exception as e:
            print_error(f"loading config file {
                abs_config_path}: {e}")
            raise

    def _get_container_image(self, config):
        """Extract container image from config."""
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            raise ValueError(
                "No container image specified in config under 'sdk.image'")
        return container_image

    def _get_target_architecture(self, config, resolved_target):
        """Extract target architecture from resolved target or config."""
        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        if resolved_target:
            return resolved_target

        target_arch = get_target_from_config(config)
        if not target_arch:
            raise ValueError(
                "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'")
        return target_arch

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk run command."""
        try:
            if args.interactive and args.detach:
                print(
                    "Error: Cannot specify both --interactive (-i) and --detach (-d) simultaneously.", file=sys.stderr)
                if parser:
                    parser.print_help()
                return False

            # Require either a command or --interactive flag
            if not args.interactive and not args.command:
                print(
                    "Error: You must either provide a command or use --interactive (-i).", file=sys.stderr)
                if parser:
                    parser.print_help()
                return False

            config = self._load_config(args.config)
            container_image = self._get_container_image(config)
            target_arch = self._get_target_architecture(config, args.resolved_target)
            container_helper = SdkContainer()

            if args.name:
                print(f"Container name: {args.name}")

            command = args.command or "bash"

            success = container_helper.run_in_container(
                container_image=container_image,
                target=target_arch,
                command=command,
                container_name=args.name,
                detach=args.detach,
                rm=args.rm,
                verbose=args.verbose,
                source_environment=False,
                interactive=args.interactive
            )

            if success:
                print_success("SDK command completed successfully.")

            return success

        except KeyboardInterrupt:
            print(
                "\nINFO: Avocado command interrupted by user (during setup or before container run).")
            return False
        except ValueError as ve:
            print_error(f"Configuration error: {ve}")
            return False
        except FileNotFoundError:
            return False
        except Exception as e:
            print_error(f"An unexpected error occurred in 'sdk run': {
                e}")
            return False
