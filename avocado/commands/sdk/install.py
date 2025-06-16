"""SDK install command implementation."""
import os
import sys
import tomlkit
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper


class SdkInstallCommand(BaseCommand):
    """Implementation of the 'sdk install' command."""

    def _resolve_package_dependencies(self, config, package_name, package_spec):
        """Resolve dependencies for a package specification."""
        dependencies = []

        if isinstance(package_spec, str):
            # Simple string version: "package-name = version"
            dependencies.append(package_name)
        elif isinstance(package_spec, dict):
            if "version" in package_spec:
                # Object with version: "package-name = { version = "1.0.0" }"
                dependencies.append(package_name)
            elif "compile" in package_spec:
                # Object with compile reference - only install the compile dependencies, not the package itself
                compile_name = package_spec["compile"]

                # Look for compile dependencies
                if "sdk" in config and "compile" in config["sdk"] and compile_name in config["sdk"]["compile"]:
                    compile_config = config["sdk"]["compile"][compile_name]
                    if "dependencies" in compile_config:
                        for dep_name, dep_spec in compile_config["dependencies"].items():
                            if isinstance(dep_spec, str):
                                dependencies.append(dep_name)
                            elif isinstance(dep_spec, dict) and "version" in dep_spec:
                                dependencies.append(dep_name)
                            # Note: compile dependencies cannot have further compile references

        return dependencies

    def _get_packages_from_config(self, config):
        """Get all packages to install from sdk.dependencies section."""
        if "sdk" not in config or "dependencies" not in config["sdk"]:
            return []

        all_packages = []
        sdk_deps = config["sdk"]["dependencies"]

        for package_name, package_spec in sdk_deps.items():
            resolved_deps = self._resolve_package_dependencies(
                config, package_name, package_spec)
            all_packages.extend(resolved_deps)

        # Remove duplicates while preserving order
        seen = set()
        unique_packages = []
        for pkg in all_packages:
            if pkg not in seen:
                seen.add(pkg)
                unique_packages.append(pkg)

        return unique_packages

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk install command's subparser."""
        parser = subparsers.add_parser(
            "install",
            help="Install SDK components"
        )

        # Add optional arguments for DNF
        parser.add_argument(
            "--reinstall",
            action="store_true",
            help="Reinstall the package"
        )

        parser.add_argument(
            "--exclude",
            help="Exclude packages by name or glob"
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

        # Add components argument - optional (when not provided, installs from config)
        parser.add_argument(
            "components",
            nargs="*",
            help="SDK components to install (if not provided, installs all from config)"
        )

        return parser

    def execute(self, args, parser=None):
        """Execute the sdk install command."""
        components = args.components
        reinstall = args.reinstall
        exclude = args.exclude
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

        # Determine what packages to install
        if components:
            # Install specific components provided as arguments
            packages_to_install = components
            print(f"Installing specific SDK components: {
                  ', '.join(components)}")
        else:
            # Install all packages from config
            packages_to_install = self._get_packages_from_config(config)
            if not packages_to_install:
                print("No SDK dependencies found in configuration.")
                return True
            print(f"Installing {len(packages_to_install)} SDK dependencies from config: {
                  ', '.join(packages_to_install)}")

        # Get the SDK image from configuration
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            print(
                "Error: No container image specified in config under 'sdk.image'", file=sys.stderr)
            return False

        # Get the target architecture from configuration
        target = config.get('runtime', {}).get('target')

        # Build the DNF command flags
        dnf_cmd_flags = "-y "

        # Add optional flags
        if reinstall:
            dnf_cmd_flags += "--reinstall "
        if exclude:
            dnf_cmd_flags += f"--exclude={exclude} "

        # Use the shared container helper to run DNF install
        container_helper = SdkContainerHelper()
        success = container_helper.run_dnf_install(
            container_image=container_image,
            packages=packages_to_install,
            target=target,
            dnf_flags=dnf_cmd_flags.strip(),
            verbose=verbose
        )

        if success:
            if components:
                print(f"Successfully installed SDK components: {
                      ', '.join(components)}")
            else:
                print(f"Successfully installed {
                      len(packages_to_install)} SDK dependencies from config")
            return True
        else:
            if components:
                print(f"Failed to install SDK components: {
                      ', '.join(components)}", file=sys.stderr)
            else:
                print("Failed to install SDK dependencies from config",
                      file=sys.stderr)
            return False
