"""Runtime deps command implementation."""

from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.output import print_error, print_success


class RuntimeDepsCommand(BaseCommand):
    """Implementation of the 'runtime deps' command."""

    def _list_runtime_dependencies(self, config, runtime_name):
        """Enumerate dependencies for a specific runtime with proper formatting."""
        if "runtime" not in config or runtime_name not in config["runtime"]:
            return []

        runtime_config = config["runtime"][runtime_name]
        if "dependencies" not in runtime_config:
            return []

        dependencies = []
        deps = runtime_config["dependencies"]

        for dep_name, dep_spec in deps.items():
            if isinstance(dep_spec, dict) and "ext" in dep_spec:
                # This is an extension reference
                ext_name = dep_spec["ext"]
                # Get version from extension config if available
                version = "*"
                if "ext" in config and ext_name in config["ext"]:
                    ext_config = config["ext"][ext_name]
                    if "version" in ext_config:
                        version = ext_config["version"]
                dependencies.append(("ext", ext_name, version))
            else:
                # This is a package
                if isinstance(dep_spec, str):
                    version = dep_spec
                elif isinstance(dep_spec, dict) and "version" in dep_spec:
                    version = dep_spec["version"]
                else:
                    version = "*"
                dependencies.append(("pkg", dep_name, version))

        # Sort: extensions first, then packages, both alphabetically
        dependencies.sort(key=lambda x: (x[0] != "ext", x[1]))
        return dependencies

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the runtime deps command's subparser."""
        parser = subparsers.add_parser("deps", help="List dependencies for a runtime")

        parser.add_argument(
            "-c",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument("runtime", help="Runtime name to list dependencies for")

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the runtime deps command."""
        config_path = args.config
        runtime_name = args.runtime

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Check if runtime section exists
        if "runtime" not in config:
            print_error(f"Runtime '{runtime_name}' not found in configuration.")
            return False
        runtime_config = config["runtime"]

        # Check if runtime exists and is a dictionary (runtime configuration)
        if runtime_name not in runtime_config or not isinstance(
            runtime_config[runtime_name], dict
        ):
            print_error(
                f"Runtime '{
                runtime_name}' not found in configuration."
            )
            return False

        # List dependencies for the runtime
        dependencies = self._list_runtime_dependencies(config, runtime_name)
        for dep_type, dep_name, dep_version in dependencies:
            print(f"({dep_type}) {dep_name} ({dep_version})")

        # Print success message with count
        count = len(dependencies)
        print_success(f"Listed {count} dependency(s).")

        return True
