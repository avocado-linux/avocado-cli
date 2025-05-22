"""SDK install command implementation."""
import os
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper
from avocado.utils.output import print_success, print_info, print_error
from avocado.utils.config import load_config


class SdkInstallCommand(BaseCommand):
    """Implementation of the 'sdk install' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk install command's subparser."""
        parser = subparsers.add_parser(
            "install",
            help="Install dependencies into the SDK"
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
            help="Enable verbose output"
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

        print_info("Installing SDK dependencies.")

        # Load the configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image and target from configuration
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            print_error(
                "No container image specified in config under 'sdk.image'")
            return False

        target = config.get('runtime', {}).get('target')
        if not target:
            print_error(
                "No target architecture specified in config under 'runtime.target'")
            return False

        # Get SDK dependencies
        sdk_config = config.get('sdk', {})
        sdk_dependencies = sdk_config.get('dependencies', {})

        # Get compile section dependencies
        compile_dependencies = self._get_compile_sections_dependencies(config)

        if not sdk_dependencies and not compile_dependencies:
            print_info(
                "No dependencies found in [sdk.dependencies] or [sdk.compile.*.dependencies].")
            return True

        overall_success = True

        # Use the container helper to run the installation
        container_helper = SdkContainerHelper()

        # Install SDK dependencies (into SDK)
        if sdk_dependencies:
            print_info(f"Found {len(sdk_dependencies)
                                } SDK dependencies to install.")

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
                print_info(f"Installing {len(sdk_packages)} package(s) into SDK: {
                           ', '.join(sdk_packages)}.")

                # Use the container helper's run_dnf_install method
                install_success = container_helper.run_dnf_install(
                    container_image=container_image,
                    packages=sdk_packages,
                    target=target,
                    dnf_flags="-y $DNF_SDK_HOST_OPTS",
                    verbose=verbose,
                    source_environment=False,
                    env_vars={
                        "RPM_CONFIGDIR": "/opt/_avocado/sdk/usr/lib/rpm"}
                )

                if install_success:
                    print_success(
                        f"Installed {len(sdk_packages)} package(s) into SDK sysroot: {os.getcwd()}/_avocado/sdk.")
                else:
                    print_error("Failed to install SDK dependencies.")
                    overall_success = False

        # Install compile section dependencies (into target-dev sysroot)
        if compile_dependencies:
            print_info(f"Found {len(compile_dependencies)
                                } compile section(s) with dependencies.")

            for section_name, dependencies in compile_dependencies.items():
                print_info(f"Installing dependencies for compile section '{
                           section_name}'.")

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
                    print_info(f"Installing {len(compile_packages)} package(s) into target-dev sysroot: {
                               ', '.join(compile_packages)}.")

                    # Use the container helper's run_dnf_install method with target-dev installroot
                    install_success = container_helper.run_dnf_install(
                        container_image=container_image,
                        packages=compile_packages,
                        target=target,
                        installroot="${AVOCADO_SDK_SYSROOTS}/target-dev",
                        dnf_flags="-y",
                        verbose=verbose,
                        source_environment=False
                    )

                    if install_success:
                        print_success(f"Installed {len(compile_packages)} package(s) for section '{
                                      section_name}' into target-dev sysroot: {os.getcwd()}/_avocado/sdk/sysroots/target-dev.")
                    else:
                        print_error(f"Failed to install dependencies for compile section '{
                                    section_name}'.")
                        overall_success = False

        return overall_success
