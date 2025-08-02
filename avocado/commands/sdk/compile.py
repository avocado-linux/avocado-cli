"""SDK compile command implementation."""

import sys
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.config import load_config
from avocado.utils.output import print_error, print_success, print_info
from avocado.utils.target import resolve_target, get_target_from_config


class SdkCompileCommand(BaseCommand):
    def _get_compile_sections_from_config(self, config):
        """Extract compile sections from configuration.

        This function collects all compile sections under [sdk.compile.*].

        Args:
            config: The loaded configuration dictionary

        Returns:
            List of compile section dictionaries, each containing:
            - name: The section name
            - script: The compile script path
            - config: The full section configuration
        """
        if "sdk" not in config or "compile" not in config["sdk"]:
            return []

        compile_sections = []
        sdk_compile = config["sdk"]["compile"]

        for section_name, section_config in sdk_compile.items():
            if isinstance(section_config, dict) and "compile" in section_config:
                compile_script = section_config["compile"]
                compile_sections.append(
                    {
                        "name": section_name,
                        "script": compile_script,
                        "config": section_config,
                    }
                )

        return compile_sections

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk compile command's subparser."""
        parser = subparsers.add_parser("compile", help="Run compile scripts")

        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "-v", "--verbose", action="store_true", help="Enable verbose output"
        )

        # Add optional compile section name argument
        parser.add_argument(
            "sections",
            nargs="*",
            help="Compile section names (if not provided, compiles all sections)",
        )

        parser.add_argument(
            "--container-args",
            nargs="*",
            help="Additional arguments to pass to the container runtime (e.g., volume mounts, port mappings)",
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk compile command."""
        sections = args.sections if hasattr(args, "sections") else []
        config_path = args.config
        verbose = args.verbose
        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get compile sections from config
        compile_sections = self._get_compile_sections_from_config(config)

        if not compile_sections:
            print_success("No compile sections configured.")
            return True

        # Filter sections if specific ones were requested
        if sections:
            requested_sections = set(sections)
            available_sections = {section["name"] for section in compile_sections}
            missing_sections = requested_sections - available_sections

            if missing_sections:
                print_error(
                    f"The following compile sections were not found: {
                    ', '.join(missing_sections)}"
                )
                print(
                    f"Available sections: {', '.join(
                    available_sections)}",
                    file=sys.stderr,
                )
                return False

            compile_sections = [
                s for s in compile_sections if s["name"] in requested_sections
            ]

        print(
            f"Found {len(compile_sections)} compile section(s) to process: {
              ', '.join([s['name'] for s in compile_sections])}"
        )

        # Get the SDK image from configuration
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

        overall_success = True

        for section in compile_sections:
            section_name = section["name"]
            compile_script = section["script"]

            print_info(
                f"Compiling section '{
                section_name}' with script '{compile_script}'"
            )

            container_helper = SdkContainer()

            compile_command = f"if [ -f '{compile_script}' ]; then echo 'Running compile script: {compile_script}'; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{
                compile_script}'; else echo 'Compile script {compile_script} not found.'; exit 1; fi"

            success = container_helper.run_in_container(
                container_image=container_image,
                target=target,
                command=compile_command,
                verbose=verbose,
                source_environment=True,
                container_args=getattr(args, 'container_args', None),
            )

            if success:
                print_success(f"Compiled section '{section_name}'.")
            else:
                print_error(
                    f"Failed to compile section '{
                    section_name}'."
                )
                overall_success = False

        if overall_success:
            print_success(
                f"All {len(compile_sections)
                                 } compile section(s) completed successfully!"
            )

        return overall_success
