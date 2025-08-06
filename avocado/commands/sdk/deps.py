"""SDK deps command implementation."""

from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.output import print_success


class SdkDepsCommand(BaseCommand):
    """Implementation of the 'sdk deps' command."""

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
                # Object with compile reference - only install the compile dependencies, not the package itself
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

    def _list_packages_from_config(self, config):
        """List all packages from SDK dependencies and compile dependencies in config."""
        if "sdk" not in config:
            return []

        all_packages = []

        # Process SDK dependencies
        if "dependencies" in config["sdk"]:
            sdk_deps = config["sdk"]["dependencies"]
            for package_name, package_spec in sdk_deps.items():
                resolved_deps = self._resolve_package_dependencies(
                    config, package_name, package_spec
                )
                all_packages.extend(resolved_deps)

        # Process compile dependencies
        if "compile" in config["sdk"]:
            compile_sections = config["sdk"]["compile"]
            for section_name, section_config in compile_sections.items():
                if (
                    isinstance(section_config, dict)
                    and "dependencies" in section_config
                ):
                    compile_deps = section_config["dependencies"]
                    for package_name, package_spec in compile_deps.items():
                        if isinstance(package_spec, str):
                            all_packages.append(
                                ("pkg", package_name, package_spec))
                        elif (
                            isinstance(
                                package_spec, dict) and "version" in package_spec
                        ):
                            all_packages.append(
                                ("pkg", package_name, package_spec["version"])
                            )

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
        """Register the sdk deps command's subparser."""
        parser = subparsers.add_parser("deps", help="List SDK dependencies")

        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk deps command."""
        config_path = args.config

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # List packages from config
        packages = self._list_packages_from_config(config)

        for dep_type, pkg_name, pkg_version in packages:
            print(f"({dep_type}) {pkg_name} ({pkg_version})")

        # Print success message with count
        count = len(packages)
        print_success(f"Listed {count} dependency(s).")

        return True
