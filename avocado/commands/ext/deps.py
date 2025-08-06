"""Extension deps command implementation."""

from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.output import print_error, print_success


class ExtDepsCommand(BaseCommand):
    """Implementation of the 'ext deps' command."""

    def _resolve_package_dependencies(self, config, package_name, package_spec):
        """Resolve dependencies for a package specification."""
        dependencies = []

        if isinstance(package_spec, str):
            # Simple string version: "package-name = version"
            dependencies.append(("pkg", package_name, package_spec))
        elif isinstance(package_spec, dict):
            if "version" in package_spec:
                # Object with version: "package-name = { version = "1.0.0" }"
                dependencies.append(
                    ("pkg", package_name, package_spec["version"]))
            elif "ext" in package_spec:
                # Extension reference
                ext_name = package_spec["ext"]
                # Get version from extension config if available
                version = "*"
                if "ext" in config and ext_name in config["ext"]:
                    ext_config = config["ext"][ext_name]
                    if "version" in ext_config:
                        version = ext_config["version"]
                dependencies.append(("ext", ext_name, version))
            elif "compile" in package_spec:
                # Object with compile reference - only list the compile dependencies, not the package itself
                compile_name = package_spec["compile"]

                # Look for compile dependencies
                if (
                    "sdk" in config
                    and "compile" in config["sdk"]
                    and compile_name in config["sdk"]["compile"]
                ):
                    compile_config = config["sdk"]["compile"][compile_name]
                    if "dependencies" in compile_config:
                        for dep_name, dep_spec in compile_config[
                            "dependencies"
                        ].items():
                            if isinstance(dep_spec, str):
                                dependencies.append(
                                    ("pkg", dep_name, dep_spec))
                            elif isinstance(dep_spec, dict) and "version" in dep_spec:
                                dependencies.append(
                                    ("pkg", dep_name, dep_spec["version"])
                                )
                            # Note: compile dependencies cannot have further compile references

        return dependencies

    def _list_packages_from_config(self, config, extension):
        """List all packages from extension dependencies in config."""
        if "ext" not in config or extension not in config["ext"]:
            return []

        ext_config = config["ext"][extension]
        all_packages = []

        # Look for dependencies section first
        if "dependencies" in ext_config:
            ext_deps = ext_config["dependencies"]
            for package_name, package_spec in ext_deps.items():
                resolved_deps = self._resolve_package_dependencies(
                    config, package_name, package_spec
                )
                all_packages.extend(resolved_deps)

        # Remove duplicates while preserving order
        seen = set()
        unique_packages = []
        for dep_type, pkg_name, pkg_version in all_packages:
            pkg_key = (dep_type, pkg_name, pkg_version)
            if pkg_key not in seen:
                seen.add(pkg_key)
                unique_packages.append((dep_type, pkg_name, pkg_version))

        # Sort: extensions first, then packages, both alphabetically
        unique_packages.sort(key=lambda x: (x[0] != "ext", x[1]))
        return unique_packages

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext deps command's subparser."""
        parser = subparsers.add_parser(
            "deps", help="List dependencies for an extension"
        )

        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "extension", help="Extension name to list dependencies for")

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext deps command."""
        config_path = args.config
        extension = args.extension

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Check if ext section exists
        if "ext" not in config:
            print_error(f"Extension '{extension}' not found in configuration.")
            return False

        # Check if extension exists
        if extension not in config["ext"]:
            print_error(f"Extension '{extension}' not found in configuration.")
            return False

        # List dependencies for the extension
        packages = self._list_packages_from_config(config, extension)

        for dep_type, pkg_name, pkg_version in packages:
            print(f"({dep_type}) {pkg_name} ({pkg_version})")

        # Print success message with count
        count = len(packages)
        print_success(f"Listed {count} dependency(s).")

        return True
