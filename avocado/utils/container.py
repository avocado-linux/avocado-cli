"""Shared container utility for SDK container operations."""

import os
import shlex
import subprocess
import sys
from typing import List, Dict, Optional, Union
from avocado.utils.output import print_error


def _flatten_container_args(container_args):
    """Handle container args from action='append' with nargs='*' - flatten nested lists and split shell-quoted args."""
    if not container_args:
        return None

    flattened = []
    for arg_group in container_args:
        if isinstance(arg_group, list):
            for arg in arg_group:
                # Split each argument string using shell-like parsing
                flattened.extend(shlex.split(arg))
        else:
            flattened.extend(shlex.split(arg_group))

    return flattened if flattened else None


def _format_command_for_display(entrypoint_script: str, user_command: str, use_entrypoint: bool) -> str:
    """Format command for display in verbose logging."""
    if not use_entrypoint:
        return user_command if user_command else ""

    if not user_command:
        return "(entrypoint only)"

    return f"(entrypoint omitted)\n{user_command}"


class SdkContainer:
    def __init__(self, container_tool: str = "docker", verbose: bool = False):
        self.container_tool = container_tool
        self.cwd = os.getcwd()
        self.verbose = verbose

    def run_in_container(
        self,
        container_image: str,
        target: str,
        command: Optional[List[str]] = None,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        env_vars: Optional[Dict[str, str]] = None,
        verbose: bool = False,
        source_environment: bool = True,
        use_entrypoint: bool = True,
        interactive: bool = False,
        container_args: Optional[List[str]] = None,
    ) -> bool:
        os.makedirs("_avocado", exist_ok=True)
        bash_cmd = ["bash", "-c"]
        cmd = ""

        # Track components separately for better logging
        entrypoint_script = ""
        user_command = ""

        # Flatten container args if they came from multiple --container-args
        container_args = _flatten_container_args(container_args)

        if use_entrypoint:
            entrypoint_script = self._create_entrypoint_script(
                source_environment)
            cmd += f"{entrypoint_script}\n"

        if command and isinstance(command, list):
            user_command = ' '.join(command)
            cmd += user_command
        elif command:
            user_command = str(command)
            cmd += user_command

        bash_cmd.append(cmd)

        # If verbose is specified for this call, override instance setting
        verbose_final = verbose or self.verbose

        try:
            container_cmd = self._build_container_command(
                container_image=container_image,
                command=bash_cmd,
                target=target,
                env_vars=env_vars,
                container_name=container_name,
                detach=detach,
                rm=rm,
                interactive=interactive,
                container_args=container_args,
            )

            return self._execute_container_command(
                container_cmd, detach, verbose_final,
                entrypoint_script, user_command, use_entrypoint
            )

        except Exception as e:
            print_error(f"Container execution failed: {e}")
            return False

    def _build_container_command(
        self,
        container_image: str,
        command: Union[str, List[str]],
        target: Optional[str] = None,
        env_vars: Optional[Dict[str, str]] = None,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        interactive: bool = False,
        container_args: Optional[List[str]] = None,
    ) -> List[str]:
        """Build the complete container command."""
        container_cmd = [self.container_tool, "run"]

        # Container options
        if rm:
            container_cmd.append("--rm")
        if container_name:
            container_cmd.extend(["--name", container_name])
        if detach:
            container_cmd.append("-d")

        if interactive:
            container_cmd.extend(["-i", "-t"])

        # Default volume mounts
        container_cmd.extend(
            [
                "-v",
                f"{self.cwd}:/opt/_avocado/src:ro",
                "-v",
                f"{self.cwd}/_avocado:/opt/_avocado:rw",
            ]
        )

        # Environment variables
        if target:
            container_cmd.extend(["-e", f"AVOCADO_SDK_TARGET={target}"])

        if env_vars:
            for key, value in env_vars.items():
                container_cmd.extend(["-e", f"{key}={value}"])

        # Add custom container arguments if provided
        if container_args:
            container_cmd.extend(container_args)

        # Add the container image
        container_cmd.append(container_image)

        # Add the command to execute
        if isinstance(command, str):
            container_cmd.append(command)
        else:
            container_cmd.extend(command)

        return container_cmd

    def _execute_container_command(
        self, container_cmd: List[str], detach: bool = False, verbose: bool = False,
        entrypoint_script: str = "", user_command: str = "", use_entrypoint: bool = True
    ) -> bool:
        try:
            if verbose:
                # Show container command with user command (omitting entrypoint)
                cmd_parts = container_cmd[:-1] if container_cmd else []
                display_command = _format_command_for_display(entrypoint_script, user_command, use_entrypoint)

                if container_cmd and len(container_cmd) >= 3 and container_cmd[-3] == "bash" and container_cmd[-2] == "-c":
                    # If it's bash -c "command", show formatted command
                    verbose_cmd = cmd_parts[:-2] + ["bash", "-c", display_command]
                elif container_cmd and isinstance(container_cmd[-1], str):
                    # If last part is a string command, show formatted command
                    verbose_cmd = cmd_parts + [display_command]
                else:
                    verbose_cmd = container_cmd

                # Format command with proper line breaks and indentation
                formatted_cmd = []
                i = 0
                while i < len(verbose_cmd):
                    arg = verbose_cmd[i]
                    if i == 0:
                        formatted_cmd.append(f"  {arg}")  # docker
                    elif i == 1:
                        formatted_cmd.append(f" \\\n    {arg}")  # run
                    else:
                        # Check if this is a flag that takes a value
                        if arg.startswith('-') and i + 1 < len(verbose_cmd) and not verbose_cmd[i + 1].startswith('-'):
                            # Combine flag with its value
                            formatted_cmd.append(f" \\\n      {arg} {verbose_cmd[i + 1]}")
                            i += 1  # Skip the value since we already included it
                        else:
                            # For multi-line arguments (like the truncated command), handle them specially
                            if '\n' in arg:
                                # Indent each line of the multi-line argument
                                lines = arg.split('\n')
                                formatted_lines = [f" \\\n      {lines[0]}"]
                                for line in lines[1:]:
                                    formatted_lines.append(f"\n      {line}")
                                formatted_cmd.append(''.join(formatted_lines))
                            else:
                                formatted_cmd.append(f" \\\n      {arg}")
                    i += 1

                print(f"[DEBUG] Container command:\n{''.join(formatted_cmd)}")

            if detach:
                result = subprocess.run(
                    container_cmd, check=True, capture_output=True, text=True
                )
                container_id = result.stdout.strip()
                print(
                    f"Container started in detached mode with ID: {
                        container_id}"
                )
                return True
            else:
                result = subprocess.run(container_cmd, check=False)
                return result.returncode == 0

        except KeyboardInterrupt:
            print(
                "\nINFO: Keyboard interrupt received. Container process may also be stopping."
            )
            return False
        except subprocess.CalledProcessError as e:
            print_error(f"Container execution failed: {e}")
            if hasattr(e, "stdout") and e.stdout:
                print(f"STDOUT: {e.stdout}", file=sys.stderr)
            if hasattr(e, "stderr") and e.stderr:
                print(f"STDERR: {e.stderr}", file=sys.stderr)
            return False
        except FileNotFoundError:
            print_error(
                f"{
                    self.container_tool} command not found. Is it installed and in your PATH?"
            )
            return False

    def _create_entrypoint_script(self, source_environment: bool = True) -> str:
        script = """
set -e

# Get codename from environment or os-release
if [ -n "$AVOCADO_SDK_CODENAME" ]; then
    CODENAME="$AVOCADO_SDK_CODENAME"
else
    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        CODENAME=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    CODENAME=${CODENAME:-dev}
fi

export AVOCADO_PREFIX="/opt/_avocado/${AVOCADO_SDK_TARGET}"
export AVOCADO_SDK_PREFIX="${AVOCADO_PREFIX}/sdk"
export AVOCADO_EXT_SYSROOTS="${AVOCADO_PREFIX}/extensions"
export DNF_SDK_HOST_PREFIX="${AVOCADO_SDK_PREFIX}"
export DNF_SDK_TARGET_PREFIX="${AVOCADO_SDK_PREFIX}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$CODENAME" \
--best \
--setopt=tsflags=noscripts \
"

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${DNF_SDK_HOST_PREFIX}/var/cache \
--setopt=logdir=${DNF_SDK_HOST_PREFIX}/var/log \
--setopt=persistdir=${DNF_SDK_HOST_PREFIX}/var/lib/dnf
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export RPM_NO_CHROOT_FOR_SCRIPTS=1

if [ ! -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    echo "[INFO] Initializing Avocado SDK."
    mkdir -p $AVOCADO_SDK_PREFIX/etc
    mkdir -p $AVOCADO_EXT_SYSROOTS
    cp /etc/rpmrc $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/rpm $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/dnf $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/yum.repos.d $AVOCADO_SDK_PREFIX/etc

    mkdir -p $AVOCADO_SDK_PREFIX/usr/lib/rpm
    cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/usr/lib/rpm/

    # Before calling DNF, $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros needs to be updated to point:
    #   - /usr -> $AVOCADO_SDK_PREFIX/usr
    #   - /var -> $AVOCADO_SDK_PREFIX/var
    sed -i "s|^%_usr[[:space:]]*/usr$|%_usr                   $AVOCADO_SDK_PREFIX/usr|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros
    sed -i "s|^%_var[[:space:]]*/var$|%_var                   $AVOCADO_SDK_PREFIX/var|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros


    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_HOST_REPO_CONF -y install "avocado-sdk-$AVOCADO_SDK_TARGET"

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF check-update

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF -y install avocado-sdk-toolchain

    echo "[INFO] Installing rootfs sysroot."
    RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
      $DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF \
      -y --installroot $AVOCADO_PREFIX/rootfs install avocado-pkg-rootfs

    echo "[INFO] Installing SDK target sysroot."
    RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
    $DNF_SDK_HOST \
        $DNF_SDK_TARGET_REPO_CONF \
        -y \
        --installroot ${AVOCADO_SDK_PREFIX}/target-sysroot \
        install \
        packagegroup-core-standalone-sdk-target
fi

export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

cd /opt/_avocado/src
"""

        if source_environment:
            script += """
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi
"""
        return script
