"""SDK compile command implementation."""
import os
import sys
import tomlkit
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper


class SdkCompileCommand(BaseCommand):
    """Implementation of the 'sdk compile' command."""

    def _get_compile_sections_from_config(self, config):
        """Get all compile sections from sdk.compile configuration."""
        if "sdk" not in config or "compile" not in config["sdk"]:
            return []

        compile_sections = []
        sdk_compile = config["sdk"]["compile"]

        for section_name, section_config in sdk_compile.items():
            if isinstance(section_config, dict) and "compile" in section_config:
                compile_script = section_config["compile"]
                compile_sections.append({
                    "name": section_name,
                    "script": compile_script,
                    "config": section_config
                })

        return compile_sections

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk compile command's subparser."""
        parser = subparsers.add_parser(
            "compile",
            help="Run compile scripts defined in SDK configuration"
        )

        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        parser.add_argument(
            "-v", "--verbose",
            action="store_true",
            help="Print container command before execution"
        )

        # Add optional compile section name argument
        parser.add_argument(
            "sections",
            nargs="*",
            help="Specific compile sections to run (if not provided, runs all sections)"
        )

        return parser

    def execute(self, args, parser=None):
        """Execute the sdk compile command."""
        sections = args.sections if hasattr(args, 'sections') else []
        config_path = args.config
        verbose = args.verbose

        # Check if configuration file exists
        if not os.path.exists(config_path):
            print(f"Error: Configuration file '{
                  config_path}' not found.", file=sys.stderr)
            print(
                "Run 'avocado init' first or specify a valid configuration file with --config.", file=sys.stderr)
            return False

        # Load the configuration
        try:
            with open(config_path, "r") as f:
                config = tomlkit.parse(f.read())
        except Exception as e:
            print(f"Error loading configuration: {str(e)}", file=sys.stderr)
            return False

        # Get compile sections from config
        compile_sections = self._get_compile_sections_from_config(config)

        if not compile_sections:
            print("No compile sections found in configuration.")
            return True

        # Filter sections if specific ones were requested
        if sections:
            requested_sections = set(sections)
            available_sections = {section["name"]
                                  for section in compile_sections}
            missing_sections = requested_sections - available_sections

            if missing_sections:
                print(f"Error: The following compile sections were not found: {
                      ', '.join(missing_sections)}", file=sys.stderr)
                print(f"Available sections: {', '.join(
                    available_sections)}", file=sys.stderr)
                return False

            compile_sections = [
                s for s in compile_sections if s["name"] in requested_sections]

        print(f"Found {len(compile_sections)} compile section(s) to process: {
              ', '.join([s['name'] for s in compile_sections])}")

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

        # Process each compile section
        overall_success = True
        for section in compile_sections:
            section_name = section["name"]
            compile_script = section["script"]

            print(f"Compiling section '{
                  section_name}' with script '{compile_script}'")

            # Use the shared container helper to run the compile script
            container_helper = SdkContainerHelper()

            # Build a simple command to run the compile script
            # The container helper will handle all the SDK environment setup
            compile_command = [
                "sh", "-c",
                f"if [ -f '{compile_script}' ]; then " +
                f"echo 'Running compile script: {compile_script}'; " +
                f"bash '{compile_script}'; " +
                f"else echo 'Error: Compile script {compile_script} not found in /opt'; " +
                f"exit 1; fi"
            ]

            success = container_helper.run_user_command(
                container_image=container_image,
                command=compile_command,
                target=target,
                verbose=verbose
            )

            if success:
                print(f"Successfully compiled section '{section_name}'")
            else:
                print(f"Failed to compile section '{
                      section_name}'", file=sys.stderr)
                overall_success = False

        if overall_success:
            print(f"All {len(compile_sections)
                         } compile section(s) completed successfully!")
        else:
            print(
                "Some compile sections failed. Check the output above for details.", file=sys.stderr)

        return overall_success
