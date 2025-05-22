"""Extension install command implementation."""
import os
import sys
import tomlkit
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper


class ExtInstallCommand(BaseCommand):
    """Implementation of the 'ext install' command."""

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

    def _get_packages_from_config(self, config, extension, package_key):
        """Get all packages to install from extension configuration."""
        if "ext" not in config or extension not in config["ext"]:
            return []

        ext_config = config["ext"][extension]
        all_packages = []

        # Look for dependencies section first
        if "dependencies" in ext_config:
            ext_deps = ext_config["dependencies"]
            for package_name, package_spec in ext_deps.items():
                resolved_deps = self._resolve_package_dependencies(
                    config, package_name, package_spec)
                all_packages.extend(resolved_deps)

        # Also look for direct package specifications in the extension section
        for key, value in ext_config.items():
            if key not in ["version", "pre-build", "dependencies"] and isinstance(value, dict):
                if "compile" in value or "install" in value:
                    # This is a package specification
                    resolved_deps = self._resolve_package_dependencies(
                        config, key, value)
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
        """Register the ext install command's subparser."""
        parser = subparsers.add_parser(
            "install",
            help="Install extension dependencies"
        )

        # Add DNF-related arguments
        parser.add_argument(
            "-r", "--reinstall",
            action="store_true",
            help="Reinstall the package"
        )

        # Add config file argument
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

        # Add extension name argument - optional (when not provided, installs for all extensions)
        parser.add_argument(
            "extension",
            nargs="?",
            help="Name of the extension to install dependencies for (if not provided, installs for all extensions)"
        )

        # Add packages argument - optional (when not provided, installs from config)
        parser.add_argument(
            "packages",
            nargs="*",
            help="Specific packages to install (if not provided, installs all from config)"
        )

        return parser

    def execute(self, args, parser=None):
        """Execute the ext install command."""
        extension = args.extension
        packages = args.packages if hasattr(args, 'packages') else []
        reinstall = args.reinstall
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

        # Check if ext section exists
        if "ext" not in config:
            print("Error: No extensions found in configuration.", file=sys.stderr)
            return False

        # Determine which extensions to process
        if extension is None:
            # Install for all extensions
            extensions_to_process = list(config["ext"].keys())
            if not extensions_to_process:
                print("No extensions found in configuration.")
                return True
            print(f"Installing packages for all extensions: {
                  ', '.join(extensions_to_process)}")
        else:
            # Install for specific extension
            if extension not in config["ext"]:
                print(f"Error: Extension '{
                      extension}' not found in configuration.", file=sys.stderr)
                print(f"Run 'avocado ext create {
                      extension}' to create it first.", file=sys.stderr)
                return False
            extensions_to_process = [extension]

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

        # Build the DNF command flags
        dnf_cmd_flags = "-y "

        # Add optional flags
        if reinstall:
            dnf_cmd_flags += "--reinstall "

        # Process each extension
        overall_success = True
        for ext_name in extensions_to_process:
            print(f"Processing extension '{ext_name}'")

            # Determine what packages to install for this extension
            if packages and extension is not None:
                # Install specific packages provided as arguments (only for single extension)
                dependencies = packages
                print(f"Installing specific packages for extension '{
                      ext_name}': {', '.join(packages)}")
            else:
                # Install all packages from config for this extension
                dependencies = self._get_packages_from_config(
                    config, ext_name, "packages")

                if not dependencies:
                    print(f"No packages configured for extension '{ext_name}'")
                    continue

                print(f"Installing {len(dependencies)} packages from config for extension '{
                      ext_name}': {', '.join(dependencies)}")

            # Use the shared container helper to run DNF install with extension installroot
            container_helper = SdkContainerHelper()
            installroot = f"${{AVOCADO_SDK_SYSROOTS}}/extensions/{ext_name}"
            success = container_helper.run_dnf_install(
                container_image=container_image,
                packages=dependencies,
                target=target,
                installroot=installroot,
                dnf_flags=dnf_cmd_flags.strip(),
                verbose=verbose
            )

            if success:
                if packages and extension is not None:
                    print(f"Successfully installed specific packages for extension '{
                          ext_name}': {', '.join(packages)}")
                else:
                    print(f"Successfully installed {
                          len(dependencies)} packages from config for extension '{ext_name}'")
            else:
                if packages and extension is not None:
                    print(f"Failed to install specific packages for extension '{
                          ext_name}': {', '.join(packages)}", file=sys.stderr)
                else:
                    print(f"Failed to install packages from config for extension '{
                          ext_name}'", file=sys.stderr)
                overall_success = False

        return overall_success
