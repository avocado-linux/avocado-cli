"""SDK install command implementation."""

import os
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_success, print_info, print_error
from avocado.utils.config import load_config
from avocado.utils.target import resolve_target, get_target_from_config


class SdkInstallCommand(BaseCommand):
    """Implementation of the 'sdk install' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk install command's subparser."""
        parser = subparsers.add_parser(
            "install", help="Install dependencies into the SDK"
        )

        # Add config file argument
        parser.add_argument(
            "-c",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "-v", "--verbose", action="store_true", help="Enable verbose output"
        )

        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="Force the operation to proceed, bypassing warnings or confirmation prompts.",
        )

        return parser

    def _get_compile_sections_dependencies(self, config):
        """Get all dependencies from sdk.compile sections."""
        if "sdk" not in config or "compile" not in config["sdk"]:
            return {}

        compile_dependencies = {}
        sdk_compile = config["sdk"]["compile"]

        for section_name, section_config in sdk_compile.items():
            if isinstance(section_config, dict) and "dependencies" in section_config:
                compile_dependencies[section_name] = section_config["dependencies"]

        return compile_dependencies

    def execute(self, args, parser=None, unknown=None):
        """Execute the sdk install command."""
        config_path = args.config
        verbose = args.verbose

        # Load the configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image and target from configuration
        container_image = config.get("sdk", {}).get("image")
        if not container_image:
            print_error("No container image specified in config under 'sdk.image'")
            return False

        # Get repo_url from config, if it exists
        repo_url = config.get("sdk", {}).get("repo_url")

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

        print_info("Installing SDK dependencies.")

        # Get SDK dependencies
        sdk_config = config.get("sdk", {})
        sdk_dependencies = sdk_config.get("dependencies", {})

        # Get compile section dependencies
        compile_dependencies = self._get_compile_sections_dependencies(config)

        # Use the container helper to run the installation
        container_helper = SdkContainer()

        # Install SDK dependencies (into SDK)
        if sdk_dependencies:
            # Build list of packages to install
            sdk_packages = []
            for package_name, version in sdk_dependencies.items():
                if version == "*":
                    sdk_packages.append(package_name)
                elif isinstance(version, dict):
                    # Handle dictionary version format like {'core2_64': '*'}
                    sdk_packages.append(package_name)
                else:
                    sdk_packages.append(f"{package_name}-{version}")

            if sdk_packages:
                yes = "-y" if args.force else ""

                command = f"""
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
    install \
    {yes} \
    {' '.join(sdk_packages)}
"""
                # Use the container helper's run_in_container method
                install_success = container_helper.run_in_container(
                    container_image=container_image,
                    target=target,
                    command=command,
                    verbose=verbose,
                    source_environment=False,
                    interactive=not args.force,
                    repo_url=repo_url,
                )

                if install_success:
                    print_success(f"Installed SDK dependencies.")
                else:
                    print_error("Failed to install SDK package(s).")
                    return False
        else:
            print_success("No dependencies configured.")

        # Install compile section dependencies (into target-dev sysroot)
        if compile_dependencies:
            print_info("Installing SDK compile dependencies.")
            total = len(compile_dependencies)
            index = 0

            for section_name, dependencies in compile_dependencies.items():
                index += 1
                # Build list of packages to install
                compile_packages = []
                for package_name, version in dependencies.items():
                    if version == "*":
                        compile_packages.append(package_name)
                    elif isinstance(version, dict):
                        # Handle dictionary version format like {'core2_64': '*'}
                        compile_packages.append(package_name)
                    else:
                        compile_packages.append(f"{package_name}-{version}")

                if compile_packages:
                    installroot = "${AVOCADO_SDK_PREFIX}/target-sysroot"
                    yes = "-y" if args.force else ""
                    command = f"""
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    --installroot {installroot} \
    $DNF_SDK_TARGET_REPO_CONF \
    install \
    {yes} \
    {' '.join(compile_packages)}
"""

                    print_info(f"Installing ({index}/{total}) {section_name}.")

                    # Use the container helper's run_in_container method with target-dev installroot
                    install_success = container_helper.run_in_container(
                        command=command,
                        container_image=container_image,
                        target=target,
                        verbose=verbose,
                        source_environment=False,
                        interactive=not args.force,
                        repo_url=repo_url,
                    )

                    if not install_success:
                        print_error(
                            f"Failed to install dependencies for compile section '{
                                    section_name}'."
                        )
                        return False
                else:
                    print_info(
                        f"({index}/{total}) [sdk.compile.{section_name}.dependencies] no dependencies."
                    )

            print_success("Installed SDK compile dependencies.")

        return True
