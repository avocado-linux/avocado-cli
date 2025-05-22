"""Runtime list command implementation."""
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config


class RuntimeListCommand(BaseCommand):
    """Implementation of the 'runtime list' command."""

    def _list_runtimes_from_config(self, config):
        """List all runtime configurations from config."""
        if "runtime" not in config:
            return []

        runtimes = []
        runtime_config = config["runtime"]

        # Only look for runtime configurations (nested dictionaries)
        # Skip simple key-value pairs like target, image, version
        for key, value in runtime_config.items():
            if isinstance(value, dict):
                # This is a runtime configuration
                runtimes.append((key, str(value)))

        return runtimes

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
        """Register the runtime list command's subparser."""
        parser = subparsers.add_parser(
            "list",
            help="List runtime names"
        )

        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the runtime list command."""
        config_path = args.config

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Check if runtime section exists
        if "runtime" not in config:
            return True

        # List all runtime names
        runtimes = self._list_runtimes_from_config(config)

        for runtime_name, runtime_value in runtimes:
            print(runtime_name)

        return True
